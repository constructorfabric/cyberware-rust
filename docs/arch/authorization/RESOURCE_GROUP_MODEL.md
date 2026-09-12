<!-- Updated: 2026-04-07 by Constructor Tech -->

# Resource Group Model — AuthZ Perspective

This document describes how Gears' authorization system uses Resource Groups (RG) for access control. For the full RG gear design (domain model, API contracts, database schemas, type system), see [RG Technical Design](../../../gears/system/resource-group/docs/DESIGN.md).

---

## Overview

Gears use **resource groups** as an optional organizational layer for grouping resources. The primary purpose from the AuthZ perspective is **access control** — granting permissions at the group level rather than per-resource.

```
Tenant T1
├── [Group A]
│   ├── Resource 1
│   ├── Resource 2
│   └── [Group A.1]
│       └── Resource 3
├── [Group B]
│   ├── Resource 1
│   └── Resource 4
└── (ungrouped resources)
```

Key principles:
- **Optional** — resources may exist without group membership
- **Many-to-many** — a resource can belong to multiple groups
- **Hierarchical** — groups form a strict forest (single parent, no cycles); **multiple roots are allowed**, but among them there is **at most one tenant-type root** (the "main tenant"). All other tenants are sub-tenants below that main tenant; non-tenant roots may coexist and carry the main tenant's `tenant_id` but are not tenants themselves — see [RG DESIGN §Tenant Root Uniqueness](../../../gears/system/resource-group/docs/DESIGN.md#tenant-root-uniqueness).
- **Tenant-scoped** — groups exist within tenant boundaries
- **Typed** — groups have dynamic GTS types with configurable parent/membership rules

For topology details (forest invariants, type system, query profiles), see [RG DESIGN §Domain Model](../../../gears/system/resource-group/docs/DESIGN.md#31-domain-model).

---

## How AuthZ Uses Resource Groups

AuthZ consumes RG data as a **PIP (Policy Information Point)** source. RG is policy-agnostic — it stores hierarchy and membership data without evaluating access decisions. AuthZ plugin reads this data to resolve group-based predicates.

### Projection Tables

RG tables are the canonical source of truth, owned by the RG gear. External consumers (AuthZ resolver, domain services) may maintain **projection copies** in their databases — synchronized from RG via read contracts (`ResourceGroupReadHierarchy`).

**Projectable tables:**

- **`resource_group`** — group entities with hierarchy (`parent_id`) and tenant scope (`tenant_id`)
- **`resource_group_closure`** — pre-computed ancestor-descendant pairs with depth, enabling efficient subtree queries
- **`resource_group_membership`** — resource-to-group M:N links (see guidance below)

#### Progressive projection strategy

Whether and which tables to project depends on the deployment topology and access patterns. **Do not add projections speculatively** — each projection creates an additional database, synchronization load, and operational complexity.

| Deployment | Recommended projections | Rationale |
|------------|------------------------|-----------|
| **Monolith** (single shared DB) | **None** — all tables are already co-located | PEP JOINs against canonical tables directly; no extra databases or sync needed |
| **Microservices** (separate DBs, typical case) | **`resource_group` + `resource_group_closure`** | Supports local hierarchy operations. Authorization must use explicit `in` predicates expanded by a capable PDP because the membership table is absent. Hierarchy tables are small (~100 K rows). |
| **Microservices** with membership filtering/pagination | **`resource_group` + `resource_group_closure` + `resource_group_membership` + `gts_type`** | Only when profiling confirms the two-request pattern (RG API → domain service) is unacceptable for latency budget. `gts_type` is required to resolve external member-handle types to RG-local membership discriminators. The membership table grows as `M_resources × N_groups_per_resource` and is expected to be **10× or more larger** than hierarchy tables — see [RG DESIGN §Storage Estimates](../../../gears/system/resource-group/docs/DESIGN.md#storage-estimates) for concrete numbers |

> **Important:** When a domain service query includes filters by resource group attributes (e.g., `GET /tasks?status=pending&project={projectX}&after=…&limit=50`), the two-request pattern means N additional round-trips to the RG Membership API (one per filter page or group), not just +1. If this N-request fan-out violates the latency budget, that is the signal to project the membership table locally.
>
> **Architecture guidance:** default to consuming explicit `in` predicates expanded by the PDP. Advertise a native group capability only when every table required to execute its SQL is co-located or projected into the querying service's database.

| Native predicate | Capabilities advertised together | Required local tables |
|------------------|----------------------------------|-----------------------|
| `in_group` | `GroupMembership` | `resource_group_membership`, `gts_type` |
| `in_group_subtree` | `GroupMembership`, `GroupHierarchy` | `resource_group_membership`, `gts_type`, `resource_group_closure` |

`GroupHierarchy` is not independently executable: subtree membership also needs
`GroupMembership` and all of its tables. A service MUST omit either capability
when its required tables are unavailable. Capability omission asks the PDP to
expand the group scope to explicit resource-ID `in` predicates; the PEP does not
perform this expansion automatically. A PDP that cannot expand the scope MUST
deny rather than emit an unadvertised native predicate or remove group filtering.

For native group predicates, the PEP resource descriptor MUST also configure its
RG member-handle GTS path with
`ResourceType::with_group_membership_type(...)`. The mapping is necessary but
not evidence that projection tables exist: the service owner remains responsible
for configuring `PolicyEnforcer::with_capabilities(...)` to match the local
schema. SecureORM resolves the external path through RG's local `gts_type` table
and adds the resulting `gts_type_id` condition to the membership subquery. This
is required because `resource_group_membership` is shared by all member types
and external resource IDs are only unique within a type. The AuthZ resource name
must not be used as an implicit replacement: a gear may deliberately use a
different GTS path for policy matching. The configured value MUST be a member
type actually registered in RG's type registry (a `gts.cf.core.rg.type.v1~`-prefixed
code accepted by `validate_type_code`, whose resolved `gts_type_id` is stored
on membership rows) — a value that never resolves through `gts_type` makes
every native group predicate silently match zero rows. Resource groups themselves have no member-handle type:
group nesting is hierarchy (`parent_id`/`resource_group_closure`), not
membership, so RG's own group resource descriptor deliberately carries no
mapping.

A resource descriptor without the mapping suppresses configured group
capabilities for that request. The PEP also rejects a native group predicate
that was not among the capabilities actually advertised or that lacks the
trusted mapping.

Native group predicates currently target only `id`: one resource descriptor
carries one RG member-handle type, so applying that mapping to another property
could compare identifiers from unrelated resource types. Every group predicate
must also have an `owner_tenant_id` predicate in the same AND constraint. Keeping
the tenant predicate in a separate OR branch would let the group branch escape
the platform's mandatory tenant boundary and is rejected fail-closed.

The native SQL casts the querying entity's ID to text, rather than casting RG's
opaque `resource_id` to UUID, because non-UUID member identifiers are valid.
The comparison is intentionally an exact textual comparison: consuming gears
must write membership IDs in the same canonical representation produced by the
entity column's text cast. UUID-backed resources should use lowercase hyphenated
UUID strings (for example, `Uuid::to_string()`); RG treats IDs as opaque and does
not normalize uppercase, brace, URN, or unhyphenated variants. Text-backed ID
columns and projections must use deterministic, case-sensitive equality so
identifiers that differ by case are not conflated.

Casting the entity key can prevent PostgreSQL from using its ordinary native-type
index for the group condition alone. Deployments that enable native predicates
for large tables should confirm plans with `EXPLAIN`. When group filtering is
selective within a large tenant, a composite expression index matching the
physical tenant and resource columns lets PostgreSQL drive the entity lookup
from the membership result instead of scanning all tenant rows:

```sql
CREATE INDEX resource_tenant_id_text_idx
    ON resource_table (tenant_id, (CAST(id AS text)));
```

The exact table and column names belong in the consuming gear's migration; the
generic toolkit cannot create this index on behalf of arbitrary entities.

- RG canonical table schemas: [RG DESIGN §Database Schemas](../../../gears/system/resource-group/docs/DESIGN.md#37-database-schemas--tables)
- When to use which table: [AUTHZ_USAGE_SCENARIOS §Choosing Projection Tables](./AUTHZ_USAGE_SCENARIOS.md#choosing-projection-tables)

### Access Inheritance

- **Explicit membership, inherited access** — a resource is added to a specific group (explicit). Access is inherited top-down: a user with access to parent group G1 can access resources in all descendant groups via `in_group_subtree` predicate.
- **Flat group access** — `in_group` predicate checks direct membership only (no hierarchy traversal).

### Integration Path

AuthZ plugin reads RG hierarchy via `ResourceGroupReadHierarchy` trait (narrow, hierarchy-only read contract). In monolith deployments (current `p1` reality), it's a direct in-process call via `ClientHub` and the trait surface itself bypasses `PolicyEnforcer` (the plugin cannot evaluate itself). In a future microservice deployment (`p2`, deferred / not implemented yet), the same trait will be backed by an MTLS-authenticated request to the RG service. See [RG DESIGN §RG Authentication Modes: JWT vs MTLS](../../../gears/system/resource-group/docs/DESIGN.md#rg-authentication-modes-jwt-vs-mtls).

---

## Relationship with Tenant Model

**Tenants** and **Resource Groups** serve different purposes:

| Aspect | Tenant | Resource Group |
|--------|--------|----------------|
| **Purpose** | Ownership, isolation, billing | Grouping for access control |
| **Scope** | System-wide | Per-tenant |
| **Resource relationship** | Ownership (1:N) | Membership (M:N) |
| **Hierarchy** | Single-root tree | Forest (multiple roots per tenant) |
| **Type system** | Fixed (built-in tenant type) | Dynamic (GTS-based, vendor-defined types) |

Resource groups operate **within** tenant boundaries — groups are tenant-scoped, cross-tenant groups are forbidden, and authorization always includes a tenant constraint alongside group predicates.

**Key rules:**

1. **Groups are tenant-scoped** — a group belongs to exactly one tenant
2. **Cross-tenant groups are forbidden** — a group cannot span multiple tenants
3. **Tenant constraint always applies** — authorization always includes a tenant constraint alongside group predicates

**Further reading:**

- Tenant topology, barriers, closure tables: [TENANT_MODEL.md](./TENANT_MODEL.md)
- Tenant-hierarchy-compatible validation on group writes: [RG DESIGN §Tenant Scope for Ownership Graph](../../../gears/system/resource-group/docs/DESIGN.md#tenant-scope-for-ownership-graph)
- Tenant constraint compilation: [DESIGN.md](./DESIGN.md)

---

## References

- [RG Technical Design](../../../gears/system/resource-group/docs/DESIGN.md) — Full RG gear design (domain model, API, database schemas, security, auth modes)
- [RG PRD](../../../gears/system/resource-group/docs/PRD.md) — Product requirements
- [RG OpenAPI](../../../gears/system/resource-group/docs/openapi.yaml) — REST API specification
- [DESIGN.md](./DESIGN.md) — Core authorization design
- [TENANT_MODEL.md](./TENANT_MODEL.md) — Tenant topology, barriers, closure tables
- [AUTHZ_USAGE_SCENARIOS.md](./AUTHZ_USAGE_SCENARIOS.md) — Authorization scenarios with resource group examples
