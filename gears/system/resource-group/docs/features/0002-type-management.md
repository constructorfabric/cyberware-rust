<!-- Created: 2026-04-07 by Constructor Tech -->
<!-- Updated: 2026-04-20 by Constructor Tech -->

# Feature: GTS Type Management

- [x] `p1` - **ID**: `cpt-cf-resource-group-featstatus-type-management`

- [x] `p1` - `cpt-cf-resource-group-feature-type-management`

<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Create Type](#create-type)
  - [Update Type](#update-type)
  - [Delete Type](#delete-type)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Type Input Validation](#type-input-validation)
  - [Hierarchy Safety Check for Type Update](#hierarchy-safety-check-for-type-update)
  - [Type Seeding](#type-seeding)
- [4. States (CDSL)](#4-states-cdsl)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Type Service CRUD](#type-service-crud)
  - [Type REST Handlers](#type-rest-handlers)
  - [Type Data Seeding](#type-data-seeding)
  - [Unit Test Coverage for Type Management](#unit-test-coverage-for-type-management)
  - [Seeding Tests](#seeding-tests)
  - [Error Conversion Chain Tests](#error-conversion-chain-tests)
- [6. Acceptance Criteria](#6-acceptance-criteria)
- [7. Unit Test Plan](#7-unit-test-plan)
  - [Type Management Test Cases](#type-management-test-cases)
  - [Error Conversions](#error-conversions)
  - [Type `metadata_schema` Storage Logic](#type-metadata_schema-storage-logic)
  - [Attack Vectors on `metadata_schema`](#attack-vectors-on-metadata_schema)
  - [GTS-Specific Logic Tests](#gts-specific-logic-tests)
  - [Seeding — Types and Groups](#seeding--types-and-groups)
  - [Junction Table Assertions (`gts_type_allowed_parent`, `gts_type_allowed_membership`)](#junction-table-assertions-gts_type_allowed_parent-gts_type_allowed_membership)

<!-- /toc -->

## 1. Feature Context

### 1.1 Overview

Full lifecycle management for GTS resource group types: create, list, get, update, and delete type definitions with code format validation, case-insensitive uniqueness enforcement, hierarchy-safe update checks, delete-if-unused policy, and idempotent type seeding for deployment bootstrapping.

### 1.2 Purpose

Types define the structural rules for the resource group hierarchy — which parent-child relationships are allowed, which resource types can be members, and whether a type permits root placement. This feature enables runtime-configurable type governance through API and seed data.

**Requirements**: `cpt-cf-resource-group-fr-manage-types`, `cpt-cf-resource-group-fr-validate-type-code`, `cpt-cf-resource-group-fr-reject-duplicate-type`, `cpt-cf-resource-group-fr-seed-types`, `cpt-cf-resource-group-fr-validate-type-update-hierarchy`, `cpt-cf-resource-group-fr-delete-type-only-if-empty`

**Principles**: `cpt-cf-resource-group-principle-dynamic-types`

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-resource-group-actor-instance-administrator` | Manages type definitions via REST API, operates type seeding |
| `cpt-cf-resource-group-actor-apps` | Programmatic type management via `ResourceGroupClient` SDK |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md) — sections 5.1, 8.1
- **Design**: [DESIGN.md](../DESIGN.md) — sections 3.1, 3.2 (Type Service), 3.3, 3.7 (gts_type tables)
- **DECOMPOSITION**: [DECOMPOSITION.md](../DECOMPOSITION.md) entry 2.2
- **Dependencies**: Feature 0001 — SDK traits, persistence adapter, error mapping
- **Not applicable**: UX (backend API — no user interface); COMPL (internal platform gear — no regulatory data handling); OPS observability and rollout are managed at the gear infrastructure level (DESIGN §3.7 and platform runbooks); PERF targets are set at the system level in PRD.md NFR section.

## 2. Actor Flows (CDSL)

### Create Type

- [x] `p1` - **ID**: `cpt-cf-resource-group-flow-type-mgmt-create-type`

**Actor**: `cpt-cf-resource-group-actor-instance-administrator`

**Success Scenarios**:
- Type is created with schema_id, allowed_parent_types, allowed_membership_types, and metadata_schema
- Type is immediately available for group creation

**Error Scenarios**:
- Invalid GTS type path format → Validation error
- Duplicate schema_id → TypeAlreadyExists
- Referenced allowed_parent_types type does not exist → Validation error
- Referenced allowed_membership_types type does not exist → Validation error
- Placement invariant violated (not can_be_root AND no allowed_parent_types) → Validation error

**Steps**:
1. [x] - `p1` - Actor sends POST /api/types-registry/v1/types with type definition payload - `inst-create-type-1`
2. [x] - `p1` - Validate GTS type path format via `GtsTypePath` value object - `inst-create-type-2`
3. [x] - `p1` - Validate placement invariant: `can_be_root OR len(allowed_parent_types) >= 1` - `inst-create-type-3`
4. [x] - `p1` - **IF** allowed_parent_types is non-empty - `inst-create-type-4`
   1. [x] - `p1` - DB: SELECT id FROM gts_type WHERE schema_id IN (allowed_parent_types) — verify all referenced parent types exist - `inst-create-type-4a`
   2. [x] - `p1` - **IF** any parent type not found → **RETURN** Validation error with missing type paths - `inst-create-type-4b`
5. [x] - `p1` - **IF** allowed_membership_types is non-empty - `inst-create-type-5`
   1. [x] - `p1` - DB: SELECT id FROM gts_type WHERE schema_id IN (allowed_membership_types) — verify all referenced membership types exist - `inst-create-type-5a`
   2. [x] - `p1` - **IF** any membership type not found → **RETURN** Validation error with missing type paths - `inst-create-type-5b`
6. [x] - `p1` - Resolve GTS type path to SMALLINT surrogate ID at persistence boundary - `inst-create-type-6`
7. [x] - `p1` - DB: INSERT INTO gts_type (schema_id, metadata_schema) — with uniqueness constraint on schema_id - `inst-create-type-7`
8. [x] - `p1` - **IF** unique constraint violation → **RETURN** TypeAlreadyExists with conflicting schema_id - `inst-create-type-8`
9. [x] - `p1` - DB: INSERT INTO gts_type_allowed_parent (type_id, parent_type_id) for each allowed parent - `inst-create-type-9`
10. [x] - `p1` - DB: INSERT INTO gts_type_allowed_membership (type_id, membership_type_id) for each allowed membership - `inst-create-type-10`
11. [x] - `p1` - **RETURN** created ResourceGroupType with schema_id, allowed_parent_types, allowed_membership_types, can_be_root, metadata_schema - `inst-create-type-11`

### Update Type

- [x] `p1` - **ID**: `cpt-cf-resource-group-flow-type-mgmt-update-type`

**Actor**: `cpt-cf-resource-group-actor-instance-administrator`

**Success Scenarios**:
- Type definition updated (allowed_parent_types, allowed_membership_types, metadata_schema)
- Existing groups remain valid under new rules

**Error Scenarios**:
- Type not found → NotFound
- Removing allowed_parent that is in use by existing groups → AllowedParentTypesViolation
- Setting can_be_root=false when root groups of this type exist → AllowedParentTypesViolation
- Referenced type does not exist → Validation error
- Placement invariant violated → Validation error

**Steps**:
1. [x] - `p1` - Actor sends PUT /api/types-registry/v1/types/{code} with updated definition - `inst-update-type-1`
2. [x] - `p1` - DB: SELECT FROM gts_type WHERE schema_id = {code} — load existing type - `inst-update-type-2`
3. [x] - `p1` - **IF** type not found → **RETURN** NotFound - `inst-update-type-3`
4. [x] - `p1` - Validate placement invariant on new values - `inst-update-type-4`
5. [x] - `p1` - Validate all referenced allowed_parent_types and allowed_membership_types types exist - `inst-update-type-5`
6. [x] - `p1` - Invoke hierarchy safety check algorithm for allowed_parent_types and can_be_root changes - `inst-update-type-6`
7. [x] - `p1` - **IF** hierarchy safety check fails → **RETURN** AllowedParentTypesViolation with violating group details - `inst-update-type-7`
8. [x] - `p1` - DB: DELETE FROM gts_type_allowed_parent WHERE type_id = {id} — clear old parents - `inst-update-type-8`
9. [x] - `p1` - DB: INSERT INTO gts_type_allowed_parent — insert new parents - `inst-update-type-9`
10. [x] - `p1` - DB: DELETE FROM gts_type_allowed_membership WHERE type_id = {id} — clear old memberships - `inst-update-type-10`
11. [x] - `p1` - DB: INSERT INTO gts_type_allowed_membership — insert new memberships - `inst-update-type-11`
12. [x] - `p1` - DB: UPDATE gts_type SET metadata_schema = {new}, updated_at = now() - `inst-update-type-12`
13. [x] - `p1` - **RETURN** updated ResourceGroupType - `inst-update-type-13`

### Delete Type

- [x] `p1` - **ID**: `cpt-cf-resource-group-flow-type-mgmt-delete-type`

**Actor**: `cpt-cf-resource-group-actor-instance-administrator`

**Success Scenarios**:
- Unused type is deleted along with its junction table entries

**Error Scenarios**:
- Type not found → NotFound
- At least one group of this type exists → ConflictActiveReferences

**Steps**:
1. [x] - `p1` - Actor sends DELETE /api/types-registry/v1/types/{code} - `inst-delete-type-1`
2. [x] - `p1` - DB: SELECT id FROM gts_type WHERE schema_id = {code} - `inst-delete-type-2`
3. [x] - `p1` - **IF** type not found → **RETURN** NotFound - `inst-delete-type-3`
4. [x] - `p1` - DB: SELECT COUNT(*) FROM resource_group WHERE gts_type_id = {type_id} - `inst-delete-type-4`
5. [x] - `p1` - **IF** count > 0 → **RETURN** ConflictActiveReferences with entity count - `inst-delete-type-5`
6. [x] - `p1` - DB: DELETE FROM gts_type WHERE id = {type_id} — CASCADE deletes junction table rows - `inst-delete-type-6`
7. [x] - `p1` - **RETURN** success (204 No Content) - `inst-delete-type-7`

## 3. Processes / Business Logic (CDSL)

### Type Input Validation

- [x] `p1` - **ID**: `cpt-cf-resource-group-algo-type-mgmt-validate-type-input`

**Input**: Type create/update payload (`schema_id`, `allowed_parent_types`, `allowed_membership_types`, `can_be_root`, `metadata_schema`). Whether the type creates a new tenant scope is derived from the code prefix (`TENANT_RG_TYPE_PATH`), not from a request field.

**Output**: Validated type definition or validation error with field-level details

**Steps**:
1. [x] - `p1` - Validate `schema_id` via GtsTypePath value object (format, length, non-empty) - `inst-val-input-1`
2. [x] - `p1` - **IF** `schema_id` does not have RG type prefix `gts.cf.core.rg.type.v1~` - `inst-val-input-2`
   1. [x] - `p1` - **RETURN** Validation error: "Type schema_id must have RG type prefix" - `inst-val-input-2a`
3. [x] - `p1` - Validate placement invariant: `can_be_root == true OR len(allowed_parent_types) >= 1` - `inst-val-input-3`
4. [x] - `p1` - **IF** invariant violated - `inst-val-input-4`
   1. [x] - `p1` - **RETURN** Validation error: "Type must allow root placement or have at least one allowed parent" - `inst-val-input-4a`
5. [x] - `p1` - **FOR EACH** parent_path in allowed_parent_types - `inst-val-input-5`
   1. [x] - `p1` - Validate parent_path has RG type prefix `gts.cf.core.rg.type.v1~` - `inst-val-input-5a`
   2. [x] - `p1` - Verify parent_path exists in gts_type table - `inst-val-input-5b`
6. [x] - `p1` - **FOR EACH** membership_path in allowed_membership_types - `inst-val-input-6`
   1. [x] - `p1` - Validate membership_path is a valid GtsTypePath (no RG prefix requirement) - `inst-val-input-6a`
   2. [x] - `p1` - Verify membership_path exists in gts_type table - `inst-val-input-6b`
7. [x] - `p1` - **IF** metadata_schema provided, validate it is a valid JSON Schema via `jsonschema::validator_for()` (compile-check). Runtime metadata validation against group instances uses `validate_metadata_via_gts()` through `TypesRegistryClient`. - `inst-val-input-7`
8. [x] - `p1` - **RETURN** validated type definition - `inst-val-input-8`

### Hierarchy Safety Check for Type Update

- [x] `p1` - **ID**: `cpt-cf-resource-group-algo-type-mgmt-check-hierarchy-safety`

**Input**: Existing type definition, proposed new `allowed_parent_types` and `can_be_root` values

**Output**: Pass or AllowedParentTypesViolation with conflicting group details

**Steps**:
1. [x] - `p1` - Compute removed parent types: `old_allowed_parent_types - new_allowed_parent_types` - `inst-hier-check-1`
2. [x] - `p1` - **FOR EACH** removed_parent_type in removed set - `inst-hier-check-2`
   1. [x] - `p1` - DB: SELECT rg.id, rg.name FROM resource_group rg JOIN resource_group parent ON rg.parent_id = parent.id WHERE rg.gts_type_id = {this_type_id} AND parent.gts_type_id = {removed_parent_type_id} - `inst-hier-check-2a`
   2. [x] - `p1` - **IF** any groups found → collect as violations - `inst-hier-check-2b`
3. [x] - `p1` - **IF** can_be_root changed from true to false - `inst-hier-check-3`
   1. [x] - `p1` - DB: SELECT id, name FROM resource_group WHERE gts_type_id = {this_type_id} AND parent_id IS NULL - `inst-hier-check-3a`
   2. [x] - `p1` - **IF** any root groups found → collect as violations - `inst-hier-check-3b`
4. [x] - `p1` - **IF** violations collected → **RETURN** AllowedParentTypesViolation with violating group IDs, names, and constraint details - `inst-hier-check-4`
5. [x] - `p1` - **RETURN** pass - `inst-hier-check-5`

### Type Seeding

- [x] `p1` - **ID**: `cpt-cf-resource-group-algo-type-mgmt-seed-types`

**Input**: List of type seed definitions from deployment configuration

**Output**: Seed result (types created, types updated, unchanged count)

**Steps**:
1. [x] - `p1` - Load seed definitions from configuration source - `inst-seed-1`
2. [x] - `p1` - **FOR EACH** seed_def in seed definitions (types are independent — SHOULD be executed in parallel via `JoinSet` for throughput) - `inst-seed-2`
   1. [x] - `p1` - DB: SELECT FROM gts_type WHERE schema_id = {seed_def.schema_id} - `inst-seed-2a`
   2. [x] - `p1` - **IF** type exists AND definition matches → skip (unchanged) - `inst-seed-2b`
   3. [x] - `p1` - **IF** type exists AND definition differs → update type via update flow - `inst-seed-2c`
   4. [x] - `p1` - **IF** type does not exist → create type via create flow - `inst-seed-2d`
3. [x] - `p1` - **RETURN** seed result: {created: N, updated: N, unchanged: N} - `inst-seed-3`

## 4. States (CDSL)

Not applicable. Types are configuration entities without lifecycle states. A type either exists or does not exist — there are no intermediate states or transitions. Type availability is governed by create/delete operations, not state machines.

## 5. Definitions of Done

### Type Service CRUD

- [x] `p1` - **ID**: `cpt-cf-resource-group-dod-type-mgmt-service-crud`

The system **MUST** implement a Type Service that provides create, list, get, update, and delete operations for GTS resource group types with full domain validation.

**Required behavior**:
- Create: validate input, check uniqueness, persist type with junction table entries, return created type
- List: paginated query with OData `$filter` on `code` field, cursor-based pagination
- Get: retrieve single type by schema_id (GTS type path), return NotFound if absent
- Update: validate input, check hierarchy safety against existing groups, update definition atomically
- Delete: check for existing groups of this type, reject if in use, delete with cascade on junction tables

**Implements**:
- `cpt-cf-resource-group-flow-type-mgmt-create-type`
- `cpt-cf-resource-group-flow-type-mgmt-update-type`
- `cpt-cf-resource-group-flow-type-mgmt-delete-type`
- `cpt-cf-resource-group-algo-type-mgmt-validate-type-input`
- `cpt-cf-resource-group-algo-type-mgmt-check-hierarchy-safety`

**Constraints**: `cpt-cf-resource-group-constraint-surrogate-ids-internal`

**Touches**:
- DB: `gts_type`, `gts_type_allowed_parent`, `gts_type_allowed_membership`
- Entities: `ResourceGroupType`, `GtsTypePath`

### Type REST Handlers

- [x] `p1` - **ID**: `cpt-cf-resource-group-dod-type-mgmt-rest-handlers`

The system **MUST** implement REST endpoint handlers for type management under `/api/types-registry/v1/types` using OperationBuilder.

**Required endpoints**:
- `GET /types` — list types with OData `$filter` (field: `code`, operators: `eq`, `ne`, `in`) and cursor-based pagination (`cursor`, `limit`)
- `POST /types` — create type, return 201 Created with type body
- `GET /types/{code}` — get type by GTS type path, return 404 if not found
- `PUT /types/{code}` — update type, return 200 OK with updated body
- `DELETE /types/{code}` — delete type, return 204 No Content

All endpoints **MUST** resolve GTS type paths to SMALLINT surrogate IDs at the persistence boundary. No surrogate IDs in request or response bodies.

**Implements**:
- `cpt-cf-resource-group-flow-type-mgmt-create-type`
- `cpt-cf-resource-group-flow-type-mgmt-update-type`
- `cpt-cf-resource-group-flow-type-mgmt-delete-type`

**Touches**:
- API: `GET/POST /api/types-registry/v1/types`, `GET/PUT/DELETE /api/types-registry/v1/types/{code}`

### Type Data Seeding

- [x] `p1` - **ID**: `cpt-cf-resource-group-dod-type-mgmt-seeding`

The system **MUST** provide an idempotent type seeding mechanism for deployment bootstrapping.

**Required behavior**:
- Accept a list of type seed definitions from deployment configuration
- For each seed: create if missing, update if definition differs, skip if unchanged
- Seeding runs as a pre-deployment step with system SecurityContext (bypasses AuthZ)
- Repeated runs produce the same result (idempotent)
- Seeding validates all type constraints (format, placement invariant, referenced types)

**Implements**:
- `cpt-cf-resource-group-algo-type-mgmt-seed-types`

**Touches**:
- DB: `gts_type`, `gts_type_allowed_parent`, `gts_type_allowed_membership`

### Unit Test Coverage for Type Management

- [x] `p1` - **ID**: `cpt-cf-resource-group-dod-testing-type-mgmt`

All acceptance criteria from feature 0002 are covered by automated tests:
- Create with valid/invalid `allowed_parent_types` and `allowed_membership_types`
- Placement invariant enforcement
- Update with hierarchy safety checks (removed parent in use, can_be_root toggle)
- Delete with active group references blocked

### Seeding Tests

- [x] `p1` - **ID**: `cpt-cf-resource-group-dod-testing-seeding`

Integration tests for deployment bootstrapping:
- `seed_types`: create/update/skip idempotency with SeedResult tracking
- `seed_groups`: ordered hierarchy creation with closure table verification
- `seed_memberships`: create + Conflict/TenantIncompatibility skip handling

### Error Conversion Chain Tests

- [x] `p2` - **ID**: `cpt-cf-resource-group-dod-testing-error-conversions`

Extend `domain_unit_test.rs` with FROM-direction error conversions:
- `EnforcerError` (Denied, EvaluationFailed, CompileFailed) -> `DomainError::AccessDenied`
- `sea_orm::DbErr` -> `DomainError::Database`
- `toolkit_db::DbError` -> `DomainError::Database`

## 6. Acceptance Criteria

- [x] Type with valid schema_id and allowed_parent_types is created and persisted with junction table entries
- [x] Creating type with duplicate schema_id returns `TypeAlreadyExists` (409)
- [x] Creating type with invalid GTS type path format returns validation error (400) with field details
- [x] Creating type without `can_be_root` and without `allowed_parent_types` returns validation error (placement invariant)
- [x] Updating type to remove allowed_parent that is in use by existing groups returns `AllowedParentTypesViolation` (409)
- [x] Updating type to set `can_be_root=false` when root groups exist returns `AllowedParentTypesViolation` (409)
- [x] Updating type to add new allowed_parent succeeds when no existing groups violate new rules
- [x] Deleting unused type succeeds (204) and removes junction table entries via CASCADE
- [x] Deleting type with existing groups returns `ConflictActiveReferences` (409) with response body including entity count so the caller can display what prevents deletion
- [x] Type seeding creates missing types, updates changed types, skips unchanged types (idempotent)
- [x] List types endpoint supports OData `$filter` on `code` field with `eq`, `ne`, `in` operators
- [x] All REST responses use GTS type paths — no SMALLINT surrogate IDs exposed
- [x] Creating type with invalid metadata_schema (not valid JSON Schema) returns validation error (400)

---

## 7. Unit Test Plan

> General testing philosophy, patterns, and infrastructure: [`docs/toolkit_unified_system/12_unit_testing.md`](../../../../../docs/toolkit_unified_system/12_unit_testing.md).

### Type Management Test Cases

**File**: `type_service_test.rs`

Test setup: SQLite in-memory + TypeService + GroupService (for hierarchy safety tests).

#### TC-TYP-01: Create type with valid allowed_parent_types [P1]
- **Covers**: G4 (positive path), 0002-AC-1
- **Setup**: Create parent type first, then create child type referencing it
- **Assert**: Child type created, `allowed_parent_types` contains parent code

#### TC-TYP-02: Create type with non-existent allowed_parent_types [P1]
- **Covers**: G4
- **Setup**: Create type with `allowed_parent_types: ["gts.cf.core.rg.type.v1~cf.core.rg.missing.v1~"]`
- **Assert**: `DomainError::TypeNotFound` or `DomainError::Validation`

#### TC-TYP-03: Create type with non-existent allowed_membership_types [P1]
- **Covers**: G5
- **Setup**: Create type with `allowed_membership_types: ["gts.cf.core.idp.missing.v1~"]`
- **Assert**: Error (type not found)

#### TC-TYP-04: Placement invariant violation (can_be_root=false, no parents) [P1]
- **Covers**: G6, 0002-AC-4
- **Setup**: `CreateTypeRequest { can_be_root: false, allowed_parent_types: [] }`
- **Assert**: `DomainError::Validation` with "root placement or" message

#### TC-TYP-05: Update type happy path [P1]
- **Covers**: G1, 0002-AC-7
- **Setup**: Create type, then update with new `allowed_parent_types`
- **Assert**: Updated type returned with new parents

#### TC-TYP-06: Update type - remove allowed_parent in use by groups [P1]
- **Covers**: G2, 0002-AC-5
- **Setup**: Create parent type P, child type C (allowed_parent_types=[P]), create group of type P, create child group of type C under P group. Then update type C removing P from allowed_parent_types.
- **Assert**: `DomainError::AllowedParentTypesViolation` with violating group names

#### TC-TYP-07: Update type - set can_be_root=false with existing root groups [P1]
- **Covers**: G3, 0002-AC-6
- **Setup**: Create type with can_be_root=true, create root group of that type. Then update type setting can_be_root=false.
- **Assert**: `DomainError::AllowedParentTypesViolation` with root group names

#### TC-TYP-08: Update type - not found [P2]
- **Covers**: G1
- **Assert**: `DomainError::TypeNotFound`

#### TC-TYP-09: Delete type with existing groups [P1]
- **Covers**: G7, 0002-AC-9
- **Setup**: Create type, create group of that type. Delete type.
- **Assert**: `DomainError::ConflictActiveReferences`

#### TC-TYP-10: Update type - placement invariant on new values [P2]
- **Covers**: G1
- **Setup**: Update type with can_be_root=false and allowed_parent_types=[]
- **Assert**: `DomainError::Validation`

#### TC-TYP-11: Create type with self-reference in allowed_parent_types [P2]
- Type A lists itself as allowed_parent, but A doesn't exist yet during resolve_ids
- **Assert**: Error (type not found for self-reference)

#### TC-TYP-12: Create type with invalid format in allowed_parent_types[i] [P2]
- allowed_parent_types: `["wrong.prefix"]` — each parent validated via validate_type_code
- **Assert**: `DomainError::Validation` (prefix error)

#### TC-TYP-13: Delete nonexistent type [P2]
- **Assert**: `DomainError::TypeNotFound`

#### TC-TYP-14: Create type with metadata_schema [P2]
- Create type with `metadata_schema: Some(json_schema)`, get type, verify schema stored
- **Assert**: Returned type has matching metadata_schema

#### TC-TYP-15: Update type replaces allowed_membership_types [P2]
- Create type with memberships [A, B], update to [B, C]
- **Assert**: Updated type has only [B, C], A removed

#### TC-TYP-16: Update type - hierarchy check skips deleted parent type [P3]
- Remove parent type from allowed_parent_types, but the parent type itself was already deleted from system
- **Assert**: No error (resolve_id returns None → skip)

### Error Conversions

**File**: `domain_unit_test.rs` (extend existing)

Existing tests cover `DomainError -> ResourceGroupError` and `DomainError -> Problem`, but miss conversions FROM external crate errors.

#### TC-ERR-01: EnforcerError::Denied -> DomainError::AccessDenied [P1]
- **Covers**: G49
- **Assert**: Mapping produces AccessDenied variant

#### TC-ERR-02: EnforcerError::EvaluationFailed -> DomainError::InternalError [P2]
- **Covers**: G49
- **Assert**: PDP RPC failures are infrastructure faults, not denials

#### TC-ERR-03: EnforcerError::CompileFailed -> DomainError::InternalError [P2]
- **Covers**: G49
- **Assert**: Constraint compile failures map to InternalError, except the
  `UnadvertisedCapabilities` sub-case (a PDP capability-negotiation
  violation), which maps to AccessDenied (TC-ERR-03b)

#### TC-ERR-04: sea_orm::DbErr -> DomainError::Database [P2]
- **Covers**: G50
- **Assert**: `DomainError::Database { message }` with original error text

#### TC-ERR-05: toolkit_db::DbError -> DomainError::Database [P2]
- **Covers**: G51

### Type `metadata_schema` Storage Logic

**File**: `type_service_test.rs` (service-level with DB)

`type_service.rs` transforms metadata_schema on write (inject `__can_be_root` via `build_stored_schema`) and `type_repo.rs` transforms on read (strip `__` keys, derive `can_be_root`). This logic has **0 tests**.

#### TC-META-01: Type with metadata_schema Object — round-trip [P1]
- **Setup**: Create type with `metadata_schema: Some(json!({"type": "object", "properties": {"x": {"type": "string"}}}))`
- Get type → returned `metadata_schema` matches input exactly
- **DB assert**: stored JSONB contains `__can_be_root` key (internal) AND user keys
- **API assert**: response does NOT contain `__can_be_root`

#### TC-META-02: Type metadata_schema with non-Object (array) → wrap/unwrap [P1]
- `metadata_schema: Some(json!([1,2,3]))` — `build_stored_schema` wraps it: `{"__user_schema": [1,2,3], "__can_be_root": true}`
- `load_full_type` strips `__` keys → returns `[1,2,3]`? Actually: the `load_full_type` code checks `if let Value::Object(map) = ms` → for non-Object stored value, returns `Some(ms.clone())` — but stored value IS Object (`{"__user_schema": ..., "__can_be_root": ...}`), so it filters keys → returns `{}` → `None` if empty. **BUG?** The array is lost.
- **Assert**: Verify actual behavior — is non-Object metadata_schema recoverable after round-trip?

#### TC-META-03: Type metadata_schema with non-Object (string) → same wrap issue [P1]
- `metadata_schema: Some(json!("my-schema"))` → stored as `{"__user_schema": "my-schema", "__can_be_root": true}`
- Read: Object filtered → `{}` → None. **Original string lost?**
- **Assert**: Document actual behavior

#### TC-META-04: Type metadata_schema with non-Object (number) [P2]
- `metadata_schema: Some(json!(42))` → same wrap pattern
- Verify round-trip

#### TC-META-05: User sends `__can_be_root` in metadata_schema [P1]
- `metadata_schema: Some(json!({"__can_be_root": false, "myField": "value"}))`
- `build_stored_schema`: clones Object, then `map.insert("__can_be_root", Bool(can_be_root))` — **OVERWRITES** user's value
- `load_full_type`: strips `__can_be_root` → returned metadata has only `{"myField": "value"}`
- **Assert**: system's `can_be_root` wins, user's `__can_be_root` silently overwritten, `myField` preserved

#### TC-META-06: User sends `__other_internal` key in metadata_schema [P1]
- `metadata_schema: Some(json!({"__secret": "data", "visible": "ok"}))`
- Read: `__secret` stripped (starts with `__`)
- **Assert**: returned metadata is `{"visible": "ok"}` — data loss is silent

#### TC-META-07: Single underscore key `_myField` preserved [P2]
- `metadata_schema: Some(json!({"_myField": "value"}))`
- **Assert**: round-trip returns `{"_myField": "value"}` (NOT stripped — only `__` prefix)

#### TC-META-08: Type metadata_schema=None → stored with only __can_be_root [P2]
- Create type with `metadata_schema: None`
- **DB assert**: stored JSONB = `{"__can_be_root": true}` (or false)
- **API assert**: returned `metadata_schema` = None (stripped to empty → None)

#### TC-META-09: can_be_root derived from stored __can_be_root [P1]
- Create type can_be_root=true → get_type → `can_be_root == true`
- Create type can_be_root=false (with parents) → get_type → `can_be_root == false`
- **DB assert**: `__can_be_root` in stored JSONB matches

#### TC-META-10: can_be_root fallback when __can_be_root missing from stored JSON [P1]
- Manually insert gts_type row with `metadata_schema = '{}'` (no __can_be_root key)
- Type has allowed_parent_types → `can_be_root = false` (fallback: `allowed_parent_types.is_empty()`)
- Type has no allowed_parent_types → `can_be_root = true`

#### TC-META-11: metadata_schema not validated as JSON Schema [P2]
- Feature 0002 says "validate it is valid JSON Schema" (inst-val-input-7)
- Code does NOT validate — any JSON value accepted
- `metadata_schema: Some(json!({"not": "a valid json schema at all"}))` → succeeds
- **Assert**: Verify no validation (document gap vs requirement)

### Attack Vectors on `metadata_schema`

These tests verify the system is resilient to adversarial metadata_schema payloads. The key attack surface is `build_stored_schema()` which clones user input into the storage JSONB.

#### TC-META-ATK-01: Overwrite `__can_be_root` via metadata_schema to escalate privileges [P1]
- Create type with `can_be_root: false, allowed_parent_types: [P], metadata_schema: {"__can_be_root": true}`
- **Attack**: user tries to force `can_be_root=true` via injected internal key
- **Assert**: `get_type().can_be_root == false` (system wins, user's `__can_be_root` overwritten by `build_stored_schema`)

#### TC-META-ATK-02: Inject `__can_be_root` with non-boolean value [P1]
- `metadata_schema: {"__can_be_root": "maybe", "x": 1}`
- `build_stored_schema` overwrites with `Bool(can_be_root)` → no issue
- But what if stored JSONB was manually corrupted to `{"__can_be_root": "not-a-bool"}`?
- `load_full_type` calls `.as_bool()` → `None` → fallback to `allowed_parent_types.is_empty()`
- **Assert**: Verify fallback works, no panic

#### TC-META-ATK-03: Inject multiple `__` prefixed keys to pollute internal storage [P1]
- `metadata_schema: {"__can_be_root": false, "__internal_flag": true, "__secret": "admin"}`
- **Assert**: After round-trip, user gets back only non-`__` keys. Internal keys don't accumulate across updates.

#### TC-META-ATK-04: Huge metadata_schema payload (DoS via JSONB size) [P1]
- `metadata_schema: Some(json!({"x": "A".repeat(1_000_000)}))` — 1MB payload
- **Assert**: Verify behavior — does DB accept it? Is there a size limit? If not, document as risk.

#### TC-META-ATK-05: Deeply nested metadata_schema (stack overflow / parse bomb) [P1]
- `metadata_schema` with 100+ levels of nesting: `{"a": {"a": {"a": ...}}}`
- **Assert**: No panic, no stack overflow in serde/sea-orm

#### TC-META-ATK-06: metadata_schema with special JSON values [P2]
- `metadata_schema: Some(json!({"nan": f64::NAN}))` — NaN is not valid JSON
- `metadata_schema: Some(json!(null))` — top-level null
- `metadata_schema: Some(json!(true))` — top-level boolean
- **Assert**: Each case either rejected or handled gracefully (no panic, no corrupt storage)

#### TC-META-ATK-07: metadata_schema with keys that conflict with SeaORM/SQL [P2]
- Keys like `"id"`, `"schema_id"`, `"gts_type_id"`, `"tenant_id"` in metadata_schema
- **Assert**: No column collision — JSONB is isolated from relational columns

#### TC-META-ATK-10: Update type metadata_schema — verify old internal keys don't leak [P1]
- Create type with `metadata_schema: {"v1": "old"}`
- Update type with `metadata_schema: {"v2": "new"}`
- **Assert**: Stored JSONB fully replaced (no merge of old+new). `get_type` returns only `{"v2": "new"}`.
- **DB assert**: old `v1` key not present in stored JSONB

#### TC-META-ATK-11: Concurrent metadata updates don't merge [P2]
- Two updates to same type with different metadata_schema
- **Assert**: Last write wins, no partial merge

### GTS-Specific Logic Tests

**ZERO** existing tests exercise GTS path resolution, roundtrip ID↔String, or metadata internal key handling in isolation.

#### GTS Path ↔ ID Resolution

**File**: `type_service_test.rs` or `group_service_test.rs` (service-level with DB)

#### TC-GTS-01: resolve_id returns SMALLINT for existing type [P1]
- Create type, verify `resolve_id(code)` returns `Some(id)` where id is `i16`

#### TC-GTS-02: resolve_id returns None for nonexistent path [P1]
- `resolve_id("gts.cf.core.rg.type.v1~cf.core.rg.missing.v1~")` → `None`

#### TC-GTS-03: resolve_ids batch — all found [P1]
- Create 3 types, `resolve_ids([code1, code2, code3])` → `Ok(vec![id1, id2, id3])`
- **Assert**: returned IDs match, order may differ

#### TC-GTS-04: resolve_ids batch — some missing [P1]
- Create type A, `resolve_ids([A, "gts.cf.core.rg.type.v1~cf.core.rg.missing.v1~"])` → `Err(Validation("Referenced types not found: gts.cf.core.rg.type.v1~cf.core.rg.missing.v1~"))`
- **Assert**: error message lists ALL missing codes

#### TC-GTS-05: resolve_ids batch — multiple missing [P2]
- `resolve_ids(["gts.cf.core.rg.type.v1~cf.core.rg.missing1.v1~", "gts.cf.core.rg.type.v1~cf.core.rg.missing2.v1~"])` → error message contains both

#### TC-GTS-06: resolve_ids empty list [P2]
- `resolve_ids([])` → `Ok(vec![])` (early return)

#### TC-GTS-07: Full roundtrip: create type → resolve_id → resolve_type_path_from_id [P1]
- Create type with code X, resolve to ID, resolve back to path
- **Assert**: returned path == X (exact string equality)

#### TC-GTS-08: load_allowed_parent_types resolves junction → IDs → paths [P1]
- Create parent type P, child type C(allowed_parent_types=[P])
- load_allowed_parent_types(C.id) → `vec!["gts.cf...P..."]`
- **Assert**: returned path == P's code

#### TC-GTS-09: load_allowed_membership_types resolves junction → IDs → paths [P1]
- Create member type M, group type G(allowed_membership_types=[M])
- load_allowed_membership_types(G.id) → `vec!["gts.cf...M..."]`

#### can_be_root Derivation & Internal Key Handling

**File**: `type_service_test.rs` (service-level), or `type_repo.rs` in-source if repo functions are pub

#### TC-GTS-10: can_be_root derived from stored __can_be_root key [P1]
- Create type with can_be_root=true, get_type → `can_be_root == true`
- Create type with can_be_root=false (with parents), get_type → `can_be_root == false`
- **Verify via DB**: `__can_be_root` key in stored JSONB matches

#### TC-GTS-11: can_be_root fallback when __can_be_root key missing [P1]
- Manually insert row in gts_type with metadata_schema without `__can_be_root` key
- load_full_type → `can_be_root` should default to `allowed_parent_types.is_empty()`
- **Scenario**: type with parents → false; type without parents → true

#### TC-GTS-12: Internal keys stripped from metadata_schema response [P1]
- Create type with `metadata_schema: {"myField": "value"}`
- Stored JSONB will have `{"myField": "value", "__can_be_root": true}`
- get_type → returned metadata_schema is `{"myField": "value"}` (no `__can_be_root`)

#### TC-GTS-13: User key with __ prefix silently stripped [P1]
- Create type with `metadata_schema: {"__custom": "data", "normal": "ok"}`
- get_type → returned metadata_schema is `{"normal": "ok"}` only
- **Document**: double-underscore keys are reserved, silently dropped on read

#### TC-GTS-14: Single underscore key preserved [P2]
- `metadata_schema: {"_my_field": "value"}` → preserved in response

#### TC-GTS-15: metadata_schema=None → __can_be_root still stored, schema returned as None [P2]
- Create type with no metadata_schema
- DB has `{"__can_be_root": true}` → after stripping __ keys → empty object → `None`

#### TC-GTS-19: allowed_parent_types.contains() exact string match after roundtrip [P1]
- Create parent type P, child type C(allowed_parent_types=[P])
- Create group of type P (root), create child group of type C under it
- **Assert**: success — proves the path stored for P matches P's code exactly during comparison

#### TC-GTS-20: validate_type_code vs GtsTypePath length limits differ [P2]
- Domain: `validate_type_code` allows up to 1024 chars
- SDK: `GtsTypePath::new()` allows up to 255 chars
- Create type with 300-char code via service → succeeds (domain limit 1024)
- Wrap same code in GtsTypePath::new() → fails (SDK limit 255)
- **Document**: inconsistency between domain and SDK validation

### Seeding — Types and Groups

**File**: `seeding_test.rs` (integration tests with SQLite)

`seeding.rs` (189 lines) has idempotent seed logic with zero tests. Seeding is a deployment-critical path — bugs here corrupt bootstrap data.

#### TC-SEED-01: seed_types creates missing type [P1]
- **Covers**: G46, 0002-AC-10
- **Setup**: Empty DB. Seed one type definition.
- **Assert**: `result.created == 1`, type exists in DB

#### TC-SEED-02: seed_types skips unchanged type [P1]
- **Covers**: G46, 0002-AC-10
- **Setup**: Seed type, then seed again with identical definition.
- **Assert**: `result.unchanged == 1`, `result.created == 0`

#### TC-SEED-03: seed_types updates changed type [P1]
- **Covers**: G46, 0002-AC-10
- **Setup**: Seed type with can_be_root=true, then seed again with can_be_root=false + allowed_parent_types.
- **Assert**: `result.updated == 1`, type in DB reflects new values

#### TC-SEED-04: seed_types idempotent (3 runs) [P2]
- **Covers**: G46
- **Setup**: Run seed 3 times with same definitions.
- **Assert**: Run 1: all created. Run 2,3: all unchanged.

#### TC-SEED-05: seed_groups creates hierarchy with closure [P1]
- **Covers**: G47, 0003-AC-24
- **Setup**: Seed parent group, then child group (ordered).
- **Assert**: `result.created == 2`, closure rows correct

#### TC-SEED-06: seed_groups skips existing group [P1]
- **Covers**: G47
- **Setup**: Seed group, seed again.
- **Assert**: `result.unchanged == 1`

#### TC-SEED-10: seed_types with empty list [P3]
- **Assert**: `SeedResult { created: 0, updated: 0, unchanged: 0, skipped: 0 }`

#### TC-SEED-11: seed_groups wrong order (child before parent) [P2]
- Child references parent_id that doesn't exist yet → error propagates
- **Assert**: Error (GroupNotFound or similar)

### Junction Table Assertions (`gts_type_allowed_parent`, `gts_type_allowed_membership`)

| Operation | Required DB Assertions |
|-----------|----------------------|
| **Create type with parents** | Junction rows COUNT = `len(allowed_parent_types)`. Each `parent_type_id` correctly resolved from GTS path to SMALLINT. |
| **Update type (replace parents)** | Old junction rows **deleted**. New rows match new list. COUNT = `len(new_allowed_parent_types)`. |
| **Update type (replace memberships)** | `gts_type_allowed_membership` contains only new entries. |
| **Delete type (CASCADE)** | `gts_type_allowed_parent WHERE type_id` → 0. `gts_type_allowed_membership WHERE type_id` → 0. |
