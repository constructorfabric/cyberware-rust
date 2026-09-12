# Feature: Data Retention

<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
  - [1.5 Out of Scope](#15-out-of-scope)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Retention TTL Synced on Startup](#retention-ttl-synced-on-startup)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Retention Config Validation](#retention-config-validation)
  - [Startup TTL Reconciliation](#startup-ttl-reconciliation)
- [4. States (CDSL)](#4-states-cdsl)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Implement retention_period_secs config field and validation](#implement-retention_period_secs-config-field-and-validation)
  - [Implement startup TTL reconciliation](#implement-startup-ttl-reconciliation)
  - [Document retention TTL behaviour](#document-retention-ttl-behaviour)
- [6. Acceptance Criteria](#6-acceptance-criteria)
- [7. Non-Applicable Concerns](#7-non-applicable-concerns)

<!-- /toc -->

- [x] `p2` - **ID**: `cpt-cf-uc-ch-plugin-featstatus-retention-implemented`

<!-- reference to DECOMPOSITION entry -->

- [x] `p2` - `cpt-cf-uc-ch-plugin-feature-retention`

## 1. Feature Context

### 1.1 Overview

Own `usage_records` storage-growth bounding via ClickHouse's native `TTL` clause — both the `retention_period_secs` config field and the mechanism by which it takes effect at runtime. Foundation's Schema Migration creates `usage_records` with a fixed 1-year TTL default; on every `init`, `ensure_retention_ttl` compares the live TTL interval to `retention_period_secs` and issues `ALTER TABLE … MODIFY TTL` when they differ.

### 1.2 Purpose

Data Retention owns the config field, its validation bounds, the semantics of the TTL clause, and the startup reconciliation that keeps the live table TTL aligned with config. The `usage_type_catalog` is reference data and is never retention-bounded.

**Requirements**: `cpt-cf-uc-ch-plugin-fr-retention`

**Constraints**: `cpt-cf-uc-ch-plugin-constraint-no-transactions` (retention mechanism is a ClickHouse background eviction, not a synchronous delete)

### 1.3 Actors

| Actor | Role in Feature |
| --- | --- |
| `cpt-cf-uc-ch-plugin-actor-operator` | Configures `retention_period_secs` in the gear config YAML; a restart applies a changed window via startup TTL reconciliation. |
| `cpt-cf-uc-ch-plugin-actor-clickhouse` | ClickHouse server applies the `TTL` eviction in background merges; eviction is asynchronous and not a plugin-emitted operation. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md) — §5 (retention FR)
- **Design**: [DESIGN.md](../DESIGN.md) — §3.2 (Schema Migration / `ensure_retention_ttl`), §3.7 (`usage_records` TTL clause)
- **Decomposition**: `cpt-cf-uc-ch-plugin-feature-retention`
- **Depends on**: `cpt-cf-uc-ch-plugin-feature-foundation`
- **DB Table**: `cpt-cf-uc-ch-plugin-dbtable-usage-records` (TTL clause owner)

### 1.5 Out of Scope

- The DDL runner call site (`apply_migrations`) — Feature 1 (`cpt-cf-uc-ch-plugin-feature-foundation`); this feature owns `retention_period_secs` semantics and `ensure_retention_ttl`.
- Per-row expiry timing and background merge scheduling — ClickHouse-internal; not a plugin-owned code path.
- `usage_type_catalog` retention — this table is reference data and is never subject to TTL eviction.

## 2. Actor Flows (CDSL)

### Retention TTL Synced on Startup

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-flow-retention-ttl-provisioning`

**Actor**: `cpt-cf-uc-ch-plugin-actor-operator` (configures); `cpt-cf-uc-ch-plugin-actor-clickhouse` (enforces eviction)

**Success Scenarios**:

- Valid `retention_period_secs` configured: after `apply_migrations`, `ensure_retention_ttl` reconciles the live TTL to the configured window; ClickHouse applies eviction in background merges.
- Re-start with the same `retention_period_secs`: reconciliation is a no-op when the live interval already matches.
- Re-start with a different `retention_period_secs`: `ALTER TABLE … MODIFY TTL` updates the live clause.

**Error Scenarios**:

- `retention_period_secs` is zero or exceeds `MAX_RETENTION_SECS` (100 years in seconds) → config validation fails, `init` fails fast.
- `ensure_retention_ttl` cannot read `system.tables` or the alter fails → `init` fails (migration-failure metric).

**Steps**:

1. [ ] - `p2` - Operator sets `retention_period_secs` in the gear config YAML - `inst-ch-ret-1`
2. [ ] - `p2` - `ClickHousePluginConfig::validate` checks the value is in `(0, MAX_RETENTION_SECS]`; on failure → **RETURN** config validation error, `init` fails fast - `inst-ch-ret-2`
3. [ ] - `p2` - `apply_migrations` runs `CREATE TABLE IF NOT EXISTS usage_records … TTL … INTERVAL 31536000 SECOND` (fixed 1-year default) - `inst-ch-ret-3`
4. [ ] - `p2` - `ensure_retention_ttl` reads `create_table_query` from `system.tables` and parses the live TTL interval - `inst-ch-ret-4`
5. [ ] - `p2` - **IF** TTL missing or parsed seconds ≠ `retention_period_secs` **OR** live clause still wraps `created_at` in `toDateTime` — issue `ALTER TABLE usage_records MODIFY TTL created_at + INTERVAL <n> SECOND DELETE` - `inst-ch-ret-5`
6. [ ] - `p2` - ClickHouse applies the TTL eviction asynchronously in background merges — no request-path row deletion occurs - `inst-ch-ret-6`

## 3. Processes / Business Logic (CDSL)

### Retention Config Validation

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-algo-retention-config-validation`

**Input**: The raw `retention_period_secs` value from `ClickHousePluginConfig`.

**Output**: A validated retention duration, or a config validation error.

**Steps**:

1. [ ] - `p2` - Check `retention_period_secs > 0` — zero is not a valid retention window - `inst-ch-ret-val-1`
2. [ ] - `p2` - Check `retention_period_secs <= MAX_RETENTION_SECS` where `MAX_RETENTION_SECS` = `100 * 365 * 24 * 3600` (approx. 100 years in seconds) — guards against `DateTime64` overflow in the ClickHouse TTL expression - `inst-ch-ret-val-2`
3. [ ] - `p2` - **RETURN** the validated value, or a descriptive config error naming the violation - `inst-ch-ret-val-3`

### Startup TTL Reconciliation

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-algo-retention-ttl-reconciliation`

`ensure_retention_ttl` reads `system.tables.create_table_query` for `usage_records`, parses either `INTERVAL <n> SECOND` or ClickHouse's rewritten `toIntervalSecond(<n>)`, and when the live interval differs from the validated `retention_period_secs`, TTL is absent, or the live clause still wraps `created_at` in `toDateTime`, executes `ALTER TABLE usage_records MODIFY TTL created_at + INTERVAL <n> SECOND DELETE`. The alter uses a validated unsigned integer only — no caller-controlled string interpolation into DDL. The TTL expression uses the `DateTime64(6)` `created_at` column directly so expiry stays past ClickHouse's 32-bit `DateTime` ceiling (2106).

## 4. States (CDSL)

Not applicable — this feature defines no entity lifecycle state machine. ClickHouse's TTL eviction is a server-internal background process; the plugin neither initiates nor observes individual row expirations. The nearest observable state is the live TTL interval on `usage_records` (operator-observable via `SHOW CREATE TABLE` / `system.tables`).

## 5. Definitions of Done

### Implement retention_period_secs config field and validation

- [x] `p2` - **ID**: `cpt-cf-uc-ch-plugin-dod-retention-config`

The system **MUST** expose `retention_period_secs` as a required field in `ClickHousePluginConfig` with a default of `31536000` (365 days), validated to be in `(0, MAX_RETENTION_SECS]` where `MAX_RETENTION_SECS` covers approximately 100 years. An out-of-range value **MUST** fail config validation with a descriptive error before `init` proceeds.

**Implements**: `cpt-cf-uc-ch-plugin-algo-retention-config-validation`

**Touches**:

- Config: `ClickHousePluginConfig`
- Component: `cpt-cf-uc-ch-plugin-component-module`

### Implement startup TTL reconciliation

- [x] `p2` - **ID**: `cpt-cf-uc-ch-plugin-dod-retention-ttl-reconciliation`

The `migrations/0001_init.sql` DDL **MUST** bake a fixed 1-year TTL (`INTERVAL 31536000 SECOND`) into `CREATE TABLE IF NOT EXISTS usage_records`. After migration, `ensure_retention_ttl` **MUST** reconcile the live TTL to `retention_period_secs`, issuing `ALTER TABLE … MODIFY TTL` when the parsed interval differs or TTL is missing.

**Implements**: `cpt-cf-uc-ch-plugin-algo-retention-ttl-reconciliation`, `cpt-cf-uc-ch-plugin-flow-retention-ttl-provisioning`

**Touches**:

- Component: `cpt-cf-uc-ch-plugin-component-migrations`
- DB Table: `cpt-cf-uc-ch-plugin-dbtable-usage-records`

### Document retention TTL behaviour

- [x] `p2` - **ID**: `cpt-cf-uc-ch-plugin-dod-retention-operator-docs`

The plugin README **MUST** document:
1. That migration DDL defaults TTL to 1 year and that startup reconciliation applies `retention_period_secs` via `ALTER TABLE … MODIFY TTL` when needed.
2. That changing `retention_period_secs` and restarting updates the live TTL.
3. That `usage_type_catalog` is never subject to TTL eviction.
4. That ClickHouse applies eviction asynchronously in background merges; rows are not immediately deleted when the TTL threshold is crossed.

**Touches**: README.md

## 6. Acceptance Criteria

- [x] `retention_period_secs` defaults to 31536000 (365 days) and is validated to `(0, MAX_RETENTION_SECS]`; zero or over-range returns a descriptive config validation error that prevents `init` from completing.
- [x] `migrations/0001_init.sql` contains a literal 1-year TTL on `usage_records` (no `{retention_period_secs}` placeholder).
- [x] `ensure_retention_ttl` updates the live TTL when config differs and is a no-op when it already matches.
- [x] `usage_type_catalog` has no TTL clause; it is never retention-bounded.
- [x] The plugin README documents startup TTL reconciliation and async eviction.
- [x] No request-path code path issues `ALTER TABLE ... DELETE`; retention alters are init-only via `MODIFY TTL`.

## 7. Non-Applicable Concerns

- **Security — Authentication & Authorization**: Not applicable — this feature has no runtime request path; all behaviour is at `init` (config validation / TTL reconcile) and ClickHouse-background (eviction).
- **Security — Audit Trail**: Not applicable.
- **Data Privacy / Compliance**: Retention policy is the operator's compliance obligation; this feature provides the mechanism. The plugin does not interpret the compliance significance of the configured window.
- **Usability (UX)**: Not applicable — no user interface; config is YAML.
- **Observability (OPS-FDESIGN-001)**: No retention-specific metrics are defined for v1 (ClickHouse's own `system.parts` TTL columns are the authoritative signal). Feature 6 (`cpt-cf-uc-ch-plugin-feature-observability`) does not include a TTL-tracking metric in v1.
- **Concurrency**: Concurrent `init` on multiple replicas may each run `MODIFY TTL` with the same target interval; that is acceptable. Eviction itself remains ClickHouse-background.
