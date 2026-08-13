# Feature: Data Retention

<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
  - [1.5 Out of Scope](#15-out-of-scope)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Retention TTL Baked at Schema Provisioning](#retention-ttl-baked-at-schema-provisioning)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Retention Config Validation](#retention-config-validation)
  - [TTL Placeholder Substitution](#ttl-placeholder-substitution)
- [4. States (CDSL)](#4-states-cdsl)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Implement retention_period_secs config field and validation](#implement-retention_period_secs-config-field-and-validation)
  - [Implement TTL placeholder substitution in schema migration](#implement-ttl-placeholder-substitution-in-schema-migration)
  - [Document operator TTL-change procedure and v1 limitation](#document-operator-ttl-change-procedure-and-v1-limitation)
- [6. Acceptance Criteria](#6-acceptance-criteria)
- [7. Non-Applicable Concerns](#7-non-applicable-concerns)

<!-- /toc -->

- [x] `p2` - **ID**: `cpt-cf-uc-ch-plugin-featstatus-retention-implemented`

<!-- reference to DECOMPOSITION entry -->

- [x] `p2` - `cpt-cf-uc-ch-plugin-feature-retention`

## 1. Feature Context

### 1.1 Overview

Own `usage_records` storage-growth bounding via ClickHouse's native `TTL` clause — both the `retention_period_secs` config field and the mechanism by which it takes effect at runtime. Foundation's Schema Migration substitutes `{retention_period_secs}` directly into the `CREATE TABLE IF NOT EXISTS` DDL before execution, baking the operator-chosen retention window into the `TTL` clause **at first table creation only** — the `CREATE TABLE IF NOT EXISTS` re-runs as a no-op on subsequent startups, so the TTL clause reflects the value in effect at original provisioning time.

### 1.2 Purpose

Data Retention owns the config field, its validation bounds, the TTL-clause coupling to Schema Migration's `{retention_period_secs}` placeholder, and the operator-facing documentation of the v1 schema-evolution limitation (no `ALTER TABLE ... MODIFY TTL` step; retention window changes require table recreation). The `usage_type_catalog` is reference data and is never retention-bounded.

**Requirements**: `cpt-cf-uc-ch-plugin-fr-retention`

**Constraints**: `cpt-cf-uc-ch-plugin-constraint-no-transactions` (retention mechanism is a ClickHouse background eviction, not a synchronous delete)

### 1.3 Actors

| Actor | Role in Feature |
| --- | --- |
| `cpt-cf-uc-ch-plugin-actor-operator` | Configures `retention_period_secs` in the gear config YAML; responsible for table-recreation if the retention window must change post-deployment. |
| `cpt-cf-uc-ch-plugin-actor-clickhouse` | ClickHouse server applies the `TTL` eviction in background merges; eviction is asynchronous and not a plugin-emitted operation. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md) — §13 (Open Questions: TTL evolution limitation)
- **Design**: [DESIGN.md](../DESIGN.md) — §3.2 (Schema Migration component), §3.7 (`usage_records` TTL clause), §4 Deferred (schema evolution limitation)
- **Decomposition**: `cpt-cf-uc-ch-plugin-feature-retention`
- **Depends on**: `cpt-cf-uc-ch-plugin-feature-foundation`
- **DB Table**: `cpt-cf-uc-ch-plugin-dbtable-usage-records` (TTL clause owner)

### 1.5 Out of Scope

- The DDL runner and `{retention_period_secs}` substitution call site — Feature 1 (`cpt-cf-uc-ch-plugin-feature-foundation`); this feature owns the semantic definition of `retention_period_secs` and the documentation of its effect, not the DDL execution machinery.
- Per-row expiry timing and background merge scheduling — ClickHouse-internal; not a plugin-owned code path.
- `usage_type_catalog` retention — this table is reference data and is never subject to TTL eviction.
- Schema-evolution versioning design for changing the TTL post-deployment — explicitly deferred to post-v1 (PRD.md §13).

## 2. Actor Flows (CDSL)

### Retention TTL Baked at Schema Provisioning

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-flow-retention-ttl-provisioning`

**Actor**: `cpt-cf-uc-ch-plugin-actor-operator` (configures); `cpt-cf-uc-ch-plugin-actor-clickhouse` (enforces eviction)

**Success Scenarios**:

- Valid `retention_period_secs` configured: DDL runner substitutes the value into the `TTL created_at + INTERVAL <n> SECOND DELETE` clause at startup; ClickHouse applies eviction in background merges.
- Re-start with the same `retention_period_secs`: `CREATE TABLE IF NOT EXISTS` is a no-op; the TTL clause in ClickHouse reflects the value used at table-creation time (no update).

**Error Scenarios**:

- `retention_period_secs` is zero or exceeds `MAX_RETENTION_SECS` (100 years in seconds) → config validation fails, `init` fails fast.

**Steps**:

1. [ ] - `p2` - Operator sets `retention_period_secs` in the gear config YAML - `inst-ch-ret-1`
2. [ ] - `p2` - `ClickHousePluginConfig::validate` checks the value is in `(0, MAX_RETENTION_SECS]`; on failure → **RETURN** config validation error, `init` fails fast - `inst-ch-ret-2`
3. [ ] - `p2` - During `apply_migrations` (Feature 1), the DDL runner substitutes `retention_period_secs` into the `{retention_period_secs}` placeholder in the `CREATE TABLE IF NOT EXISTS usage_records ... TTL created_at + INTERVAL {retention_period_secs} SECOND DELETE` DDL - `inst-ch-ret-3`
4. [ ] - `p2` - If the table does not yet exist: ClickHouse creates it with the TTL clause containing the configured value - `inst-ch-ret-4`
5. [ ] - `p2` - If the table already exists: the `CREATE TABLE IF NOT EXISTS` executes as a no-op; the TTL clause in ClickHouse reflects whatever `retention_period_secs` was configured when the table was originally created - `inst-ch-ret-5`
6. [ ] - `p2` - ClickHouse applies the TTL eviction asynchronously in background merges — no plugin-initiated `ALTER TABLE` or request-path row deletion occurs - `inst-ch-ret-6`

## 3. Processes / Business Logic (CDSL)

### Retention Config Validation

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-algo-retention-config-validation`

**Input**: The raw `retention_period_secs` value from `ClickHousePluginConfig`.

**Output**: A validated retention duration, or a config validation error.

**Steps**:

1. [ ] - `p2` - Check `retention_period_secs > 0` — zero is not a valid retention window - `inst-ch-ret-val-1`
2. [ ] - `p2` - Check `retention_period_secs <= MAX_RETENTION_SECS` where `MAX_RETENTION_SECS` = `100 * 365 * 24 * 3600` (approx. 100 years in seconds) — guards against `DateTime64` overflow in the ClickHouse TTL expression - `inst-ch-ret-val-2`
3. [ ] - `p2` - **RETURN** the validated value, or a descriptive config error naming the violation - `inst-ch-ret-val-3`

### TTL Placeholder Substitution

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-algo-retention-ttl-substitution`

The `migrations/0001_init.sql` DDL embeds the literal string `{retention_period_secs}` as a placeholder in the `TTL` clause of the `CREATE TABLE IF NOT EXISTS usage_records` statement. The DDL runner (Feature 1's `apply_migrations`) substitutes this placeholder with the validated config value before statement execution — producing e.g. `TTL created_at + INTERVAL 31536000 SECOND DELETE`. No ClickHouse SQL expression parsing or AST manipulation is needed: a plain string substitution of a validated numeric value produces injection-safe DDL because the value is an already-validated unsigned integer.

## 4. States (CDSL)

Not applicable — this feature defines no entity lifecycle state machine. ClickHouse's TTL eviction is a server-internal background process; the plugin neither initiates nor observes individual row expirations. The nearest observable state is whether the `usage_records` table carries a TTL clause (set at creation time, operator-observable via ClickHouse system tables).

## 5. Definitions of Done

### Implement retention_period_secs config field and validation

- [x] `p2` - **ID**: `cpt-cf-uc-ch-plugin-dod-retention-config`

The system **MUST** expose `retention_period_secs` as a required field in `ClickHousePluginConfig` with a default of `31536000` (365 days), validated to be in `(0, MAX_RETENTION_SECS]` where `MAX_RETENTION_SECS` covers approximately 100 years. An out-of-range value **MUST** fail config validation with a descriptive error before `init` proceeds.

**Implements**: `cpt-cf-uc-ch-plugin-algo-retention-config-validation`

**Touches**:

- Config: `ClickHousePluginConfig`
- Component: `cpt-cf-uc-ch-plugin-component-module`

### Implement TTL placeholder substitution in schema migration

- [x] `p2` - **ID**: `cpt-cf-uc-ch-plugin-dod-retention-ttl-substitution`

The `migrations/0001_init.sql` DDL file **MUST** embed `{retention_period_secs}` in the `TTL` clause of `CREATE TABLE IF NOT EXISTS usage_records`. The DDL runner's `apply_migrations` (implemented in Feature 1) **MUST** substitute this placeholder with the validated `retention_period_secs` value before executing the DDL statements. The substitution **MUST** be a plain string replacement of a validated numeric value — no SQL injection risk.

**Implements**: `cpt-cf-uc-ch-plugin-algo-retention-ttl-substitution`, `cpt-cf-uc-ch-plugin-flow-retention-ttl-provisioning`

**Touches**:

- Component: `cpt-cf-uc-ch-plugin-component-migrations`
- DB Table: `cpt-cf-uc-ch-plugin-dbtable-usage-records`

### Document operator TTL-change procedure and v1 limitation

- [x] `p2` - **ID**: `cpt-cf-uc-ch-plugin-dod-retention-operator-docs`

The plugin README **MUST** document:
1. That the TTL clause is baked into `usage_records` at table-creation time and is not updated on subsequent startups with a different `retention_period_secs`.
2. The operator procedure for changing the retention window post-deployment: drop and recreate the table (or issue a manual `ALTER TABLE usage_records MODIFY TTL ...`) — which is a destructive data operation in the drop-and-recreate case.
3. That `usage_type_catalog` is never subject to TTL eviction.
4. That ClickHouse applies eviction asynchronously in background merges; rows are not immediately deleted when the TTL threshold is crossed.

**Touches**: README.md

## 6. Acceptance Criteria

- [x] `retention_period_secs` defaults to 31536000 (365 days) and is validated to `(0, MAX_RETENTION_SECS]`; zero or over-range returns a descriptive config validation error that prevents `init` from completing.
- [x] `migrations/0001_init.sql` contains the `{retention_period_secs}` placeholder in the `usage_records` TTL clause; `apply_migrations` substitutes it with the configured value before execution.
- [x] Re-running `init` with an already-existing table is a no-op; the TTL clause in ClickHouse reflects the value used at table-creation time.
- [x] `usage_type_catalog` has no TTL clause; it is never retention-bounded.
- [x] The plugin README documents the v1 schema-evolution limitation (no `ALTER TABLE ... MODIFY TTL` step) and the operator procedure for changing the retention window.
- [x] No request-path code path issues `ALTER TABLE ... DELETE` or `ALTER TABLE ... MODIFY TTL`; all eviction is ClickHouse-background-driven.

## 7. Non-Applicable Concerns

- **Security — Authentication & Authorization**: Not applicable — this feature has no runtime request path; all behavior is at `init` (config validation) and ClickHouse-background (eviction).
- **Security — Audit Trail**: Not applicable.
- **Data Privacy / Compliance**: Retention policy is the operator's compliance obligation; this feature provides the mechanism. The plugin does not interpret the compliance significance of the configured window.
- **Usability (UX)**: Not applicable — no user interface; config is YAML.
- **Observability (OPS-FDESIGN-001)**: No retention-specific metrics are defined for v1 (ClickHouse's own `system.parts` TTL columns are the authoritative signal). Feature 6 (`cpt-cf-uc-ch-plugin-feature-observability`) does not include a TTL-tracking metric in v1.
- **Concurrency**: Not applicable for this feature — the TTL mechanism is a schema-level clause applied by ClickHouse in the background, not a concurrent code path.
