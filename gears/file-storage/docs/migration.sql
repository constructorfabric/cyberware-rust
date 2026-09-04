-- =============================================================================
-- FileStorage — database migrations
-- =============================================================================
-- All FileStorage state lives in the `file_storage` schema of the shared
-- Gears Postgres cluster. Migrations are applied through `db-runner`
-- (see docs/toolkit_unified_system/11_database_patterns.md) at gear startup
-- by one elected replica.
--
-- The file is split into three phase sections. Each section is intended to
-- be run as a single migration unit when its phase ships:
--
--   * P1 — initial release; everything required for the P1 scope in PRD.md
--          and DESIGN.md (control-plane metadata CRUD; files = identity +
--          content_id pointer + meta_version; file_versions = immutable
--          versions with SHA-256 hash, pending/available status, backend
--          pointer; one-table custom metadata). Content moves over signed
--          URLs against the sidecar (ADR-0003); no content in the control DB.
--
--   * P2 — multipart upload, versioning, idempotency, audit and event
--          outboxes, policies, retention rules
--
--   * P3 — runtime backend configuration (supersedes the P1 static TOML)
--
-- Naming convention for migration files when split per phase by the runner:
--   202xxxxxxxxx_file_storage_p1_initial.sql
--   202xxxxxxxxx_file_storage_p2_multipart.sql
--   202xxxxxxxxx_file_storage_p2_versioning.sql
--   ... etc
-- This combined file lists the DDL in dependency order within each phase.
--
-- Implementation note (P1 gear migration):
--   * This file is the CANONICAL Postgres schema (the `file_storage` schema is
--     the production target). The SeaORM migration the gear actually runs
--     (gears/file-storage/file-storage/src/infra/storage/migrations/) uses
--     UNQUALIFIED (flat) table names — `files`, `file_versions`,
--     `files_custom_metadata` — on BOTH Postgres and SQLite, because a SeaORM
--     entity has a static `table_name` and SQLite has no schemas, so a
--     per-backend schema qualifier cannot be expressed in the entity. Behaviour
--     is identical; adopting the `file_storage` schema on Postgres is a deferred,
--     entity-attribute-only change.
--   * In the entity layer `version_id` is the SOLE primary key of file_versions
--     (it is a globally unique uuid), while the table keeps the composite
--     `(file_id, version_id)` PK below; this keeps version updates/deletes keyed
--     off a single PK column. The `is_current` <-> `files.content_id` invariant
--     (see the column comment) is maintained ATOMICALLY by the bind transaction.
--
--   * This file is a design reference kept in sync by hand; the executable
--     migrations are the SeaORM files under src/infra/storage/migrations/.
--     Do not run this SQL directly.
-- =============================================================================


-- =============================================================================
-- P1 — Initial Release
-- =============================================================================

-- Schema and extensions ------------------------------------------------------

CREATE SCHEMA IF NOT EXISTS file_storage;

-- gen_random_uuid() is used for server-side ID generation where the
-- application does not supply one. Provided by the pgcrypto extension on
-- Postgres < 13 and as a built-in from 13 onwards. The shared platform
-- runtime guarantees Postgres >= 14; this is a no-op on those versions.
CREATE EXTENSION IF NOT EXISTS pgcrypto;


-- Table: file_storage.files --------------------------------------------------
-- @cpt-cf-file-storage-dbtable-files

-- The file row is the stable logical identity. It holds NO bytes and NO
-- per-content fields (mime/size/hash/backend) — those live on the current
-- file_versions row pointed at by content_id. See ADR-0003 (sidecar data plane).
CREATE TABLE file_storage.files (
    file_id                 uuid         PRIMARY KEY  DEFAULT gen_random_uuid(),

    -- Tenant boundary. Immutable after creation (enforced at the service layer).
    tenant_id               uuid         NOT NULL,

    -- Ownership principal.
    owner_kind              text         NOT NULL
                                         CHECK (owner_kind IN ('user', 'app')),
    owner_id                uuid         NOT NULL,

    -- Display + classification.
    name                    text         NOT NULL,
    gts_file_type           text         NOT NULL,

    -- Content pointer: the version_id currently bound as the file's live
    -- content. NULL until the first bind. The content-only ETag is derived
    -- from (file_id, content_id). FK is logical (the version is in
    -- file_versions, which also FKs back to files); enforced at the service
    -- layer by the bind CAS, not as a DB constraint, to avoid the cycle.
    content_id              uuid,

    -- Monotonic counter bumped on metadata-only writes; backs If-Match-Metadata.
    meta_version            bigint       NOT NULL  DEFAULT 0
                                         CHECK (meta_version >= 0),

    -- Audit timestamps.
    created_at              timestamptz  NOT NULL  DEFAULT now(),
    last_modified_at        timestamptz  NOT NULL  DEFAULT now()
);

COMMENT ON TABLE  file_storage.files                IS 'FileStorage logical file: identity + current content pointer. Holds no bytes.';
COMMENT ON COLUMN file_storage.files.tenant_id      IS 'Tenant boundary; immutable after creation.';
COMMENT ON COLUMN file_storage.files.owner_kind     IS 'Owner principal kind: user (platform user) or app (Gear).';
COMMENT ON COLUMN file_storage.files.content_id     IS 'version_id of the current content version; NULL until first bind. Backs the ETag.';
COMMENT ON COLUMN file_storage.files.meta_version   IS 'Monotonic counter; bumped on metadata-only writes. Backs If-Match-Metadata.';

-- Indexes on files -----------------------------------------------------------

-- Covers the primary `GET /files` listing query: tenant + owner_kind + owner_id
-- with created_at descending for stable cursor pagination.
CREATE INDEX files_owner_listing_idx
    ON file_storage.files (tenant_id, owner_kind, owner_id, created_at DESC);

-- Per-tenant per-type queries (used by authorization audit, P2 policy checks).
CREATE INDEX files_tenant_gts_idx
    ON file_storage.files (tenant_id, gts_file_type);


-- Table: file_storage.file_versions ------------------------------------------
-- @cpt-cf-file-storage-dbtable-file-versions
-- One row per immutable content version. The backend object lives at
-- /{file_id}/{version_id} and is never mutated. Versioning is FileStorage-level
-- (works on any backend); the current version is files.content_id. No automatic
-- cleanup in P1 (versions accumulate; the P2 cleanup engine prunes by retention).

CREATE TABLE file_storage.file_versions (
    file_id          uuid         NOT NULL
                                  REFERENCES file_storage.files (file_id) ON DELETE CASCADE,
    version_id       uuid         NOT NULL  DEFAULT gen_random_uuid(),

    -- Per-version content properties.
    mime_type        text         NOT NULL,
    size             bigint       NOT NULL  CHECK (size >= 0),  -- 0 permitted (empty file)

    -- Content hash. P1 allow-list locked to SHA-256 per ADR-0002. ADR-0002's P2
    -- vision of widening this CHECK (hash_policy/allowed_algorithms) was
    -- superseded by ADR-0006 and never implemented: this CHECK stays locked to
    -- SHA-256 as actually shipped (see the "P2 hash-policy widening" note below).
    hash_algorithm   text         NOT NULL  DEFAULT 'SHA-256'
                                  CHECK (hash_algorithm = 'SHA-256'),
    hash_value       bytea        NOT NULL  CHECK (octet_length(hash_value) = 32),

    -- ADR-0006 content-hash mode (shipped, m20260707_000001_content_hash_modes):
    -- which of the two hash modes produced hash_value. Every pre-existing row
    -- backfills to 'whole-sha256' via the column default.
    hash_mode        text         NOT NULL  DEFAULT 'whole-sha256'
                                  CHECK (hash_mode IN ('whole-sha256', 'multipart-composite-sha256')),
    -- Set only for hash_mode = 'multipart-composite-sha256' (number of parts
    -- folded into the offset-manifest); NULL for 'whole-sha256'. Enforced by
    -- the presence CHECK below.
    part_count       integer,

    -- Lifecycle: 'pending' from pre-register until bind, then 'available'.
    status           text         NOT NULL  DEFAULT 'pending'
                                  CHECK (status IN ('pending', 'available')),
    -- True for the file's current version. This is a denormalization of
    -- files.content_id, kept deliberately: it backs the unique partial index
    -- below (cheap "is there a current version" / one-current-per-file
    -- enforcement) and the version-listing `is_current` flag without a join.
    -- INVARIANT: is_current = true on exactly the row whose version_id equals
    -- files.content_id. The bind operation MUST maintain it ATOMICALLY (single
    -- transaction): swap files.content_id (CAS on If-Match), clear the old
    -- current (is_current = false), set the new current (is_current = true).
    -- See cpt-cf-file-storage-fr-upload-file; split-brain (two currents, or a
    -- current that disagrees with content_id) is prevented by the transaction
    -- plus the unique partial index, and is covered by bind tests.
    is_current       boolean      NOT NULL  DEFAULT false,

    -- Backend pointer (immutable per version). backend_id references the
    -- BackendConfig (TOML in P1 / storage_backends_runtime in P3); backend_path
    -- is an opaque per-driver path, /{file_id}/{version_id} by convention.
    backend_id       text         NOT NULL,
    backend_path     text         NOT NULL,

    created_at       timestamptz  NOT NULL  DEFAULT now(),

    PRIMARY KEY (file_id, version_id)
);

COMMENT ON TABLE file_storage.file_versions IS
    'Immutable content versions. Backend object /{file_id}/{version_id} is never mutated; a content write is a new version + a pointer swap (files.content_id).';

-- ADR-0006: hash_mode = 'multipart-composite-sha256' <=> part_count IS NOT NULL.
ALTER TABLE file_storage.file_versions
    ADD CONSTRAINT file_versions_part_count_presence_check
        CHECK ((hash_mode = 'multipart-composite-sha256') = (part_count IS NOT NULL));

-- At most one current version per file.
CREATE UNIQUE INDEX file_versions_current_idx
    ON file_storage.file_versions (file_id)
    WHERE is_current = true;

-- Supports cleanup of abandoned pre-registered versions (P2 cleanup engine).
CREATE INDEX file_versions_pending_idx
    ON file_storage.file_versions (created_at)
    WHERE status = 'pending';

-- Recovery / debugging index on backend pointer ("which versions live on backend X?").
CREATE INDEX file_versions_backend_idx
    ON file_storage.file_versions (backend_id);

-- version_id is globally unique in practice (assigned via gen_random_uuid());
-- this index makes that a DB-enforced fact so version_hash_manifest below can
-- carry a single-column FK into file_versions despite its PK being the
-- composite (file_id, version_id). Shipped in m20260707_000001_content_hash_modes.
CREATE UNIQUE INDEX file_versions_version_id_unique_idx
    ON file_storage.file_versions (version_id);


-- Table: file_storage.version_hash_manifest ----------------------------------
-- ADR-0006 (shipped, m20260707_000001_content_hash_modes): one row per
-- hash_mode = 'multipart-composite-sha256' version, carrying the canonical
-- offset-manifest text (content-hash-modes.md §3 grammar) that
-- hash_value = sha256(manifest) was derived from. No row for a
-- 'whole-sha256' version.

CREATE TABLE file_storage.version_hash_manifest (
    version_id  uuid         NOT NULL  PRIMARY KEY
                             REFERENCES file_storage.file_versions (version_id) ON DELETE CASCADE,
    manifest    text         NOT NULL,
    created_at  timestamptz  NOT NULL  DEFAULT now()
);


-- Table: file_storage.files_custom_metadata ----------------------------------
-- @cpt-cf-file-storage-dbtable-files-custom-metadata

CREATE TABLE file_storage.files_custom_metadata (
    file_id   uuid         NOT NULL
                           REFERENCES file_storage.files (file_id) ON DELETE CASCADE,
    key       text         NOT NULL,
    value     text         NOT NULL,
    set_at    timestamptz  NOT NULL  DEFAULT now(),

    PRIMARY KEY (file_id, key)
);

COMMENT ON TABLE file_storage.files_custom_metadata IS
    'User-defined key-value pairs attached to a file. JSON Merge Patch semantics on PATCH /files/{id}: keys present overwrite, keys set to null delete, keys absent are unchanged.';


-- =============================================================================
-- P2 — Multipart Upload, Versioning, Idempotency, Outboxes, Policies, Retention
-- =============================================================================

-- P2 hash-policy widening (NOT IMPLEMENTED) ----------------------------------
-- The P1 `file_versions.hash_algorithm` CHECK stays locked to 'SHA-256' in P2
-- as actually shipped (ADR-0002). There is no `hash_policy`/`allowed_algorithms`
-- config surface in code.
-- Content-hash modes (hash_mode/part_count/version_hash_manifest) ARE shipped —
-- see ADR-0006 and the `file_versions.hash_mode`/`part_count` columns plus the
-- `version_hash_manifest` table above (migrated by
-- m20260707_000001_content_hash_modes.rs).


-- Table: file_storage.multipart_uploads --------------------------------------
-- In-flight multipart upload sessions. Created on multipart initiate
-- (which also pre-registers the pending file_versions row), one row per upload
-- session. Parts go into multipart_upload_parts.

CREATE TABLE file_storage.multipart_uploads (
    upload_id        uuid         PRIMARY KEY  DEFAULT gen_random_uuid(),
    file_id          uuid         NOT NULL
                                  REFERENCES file_storage.files (file_id) ON DELETE CASCADE,

    -- The pending file_versions row this session will finalize into (the
    -- version is pre-registered at initiate time, alongside this row).
    version_id       uuid         NOT NULL,

    -- Backend-side handle (e.g., S3 UploadId) — opaque to FileStorage.
    backend_upload_handle  text   NOT NULL,

    -- Lifecycle state.
    state            text         NOT NULL  DEFAULT 'in_progress'
                                  CHECK (state IN ('in_progress', 'completed', 'aborted')),

    -- Validation state for content-type magic-bytes check (recorded after
    -- the first uploaded part).
    declared_mime    text         NOT NULL,
    mime_validated   boolean      NOT NULL  DEFAULT false,

    -- Server-authoritative plan parameters (multipart-coordinator, P2-M3):
    -- persisted so the plan (parts/offsets) can be reconstituted for resume
    -- without a separate per-plan table. `declared_size` also lets `complete`
    -- verify actual-vs-declared without re-summing parts.
    declared_size    bigint       NOT NULL  DEFAULT 0,
    part_size        bigint       NOT NULL  DEFAULT 0,

    -- TTL for abandoned uploads. The reaper marks expired in-flight uploads
    -- as 'aborted' and asks the backend to abort, freeing storage.
    created_at       timestamptz  NOT NULL  DEFAULT now(),
    expires_at       timestamptz  NOT NULL
);

CREATE INDEX multipart_uploads_file_idx ON file_storage.multipart_uploads (file_id);
CREATE INDEX multipart_uploads_expired_idx
    ON file_storage.multipart_uploads (expires_at)
    WHERE state = 'in_progress';


-- Table: file_storage.multipart_upload_parts ---------------------------------
-- One row per uploaded part.

CREATE TABLE file_storage.multipart_upload_parts (
    upload_id        uuid         NOT NULL
                                  REFERENCES file_storage.multipart_uploads (upload_id) ON DELETE CASCADE,
    part_number      int          NOT NULL  CHECK (part_number > 0),
    -- ETag-shaped per-part identifier returned by the backend on PutPart.
    backend_etag     text         NOT NULL,
    -- Per-part hash. SHA-256 (matches file_versions.hash_algorithm).
    part_hash        bytea        NOT NULL,
    size             bigint       NOT NULL  CHECK (size >= 0),
    uploaded_at      timestamptz  NOT NULL  DEFAULT now(),

    PRIMARY KEY (upload_id, part_number)
);


-- NOTE: file_versions is a P1 table (see the P1 section above) — FileStorage-level
-- versioning works on any backend via distinct objects /{file_id}/{version_id}.
-- There is no separate P2 version table; its hash_algorithm CHECK is
-- unchanged from P1 (SHA-256 only — see the "P2 hash-policy widening" note
-- above).


-- Table: file_storage.idempotency_keys ---------------------------------------
-- Owner-scoped POST /files idempotency. A retried request with the same key
-- by the same owner returns the original response without creating a duplicate
-- file. Keys are isolated per (tenant_id, owner_kind, owner_id) to avoid
-- cross-owner leaks.

CREATE TABLE file_storage.idempotency_keys (
    tenant_id      uuid         NOT NULL,
    owner_kind     text         NOT NULL  CHECK (owner_kind IN ('user', 'app')),
    owner_id       uuid         NOT NULL,
    idempotency_key text        NOT NULL,

    -- Result snapshot: which file was produced.
    file_id        uuid         NOT NULL
                                REFERENCES file_storage.files (file_id) ON DELETE CASCADE,

    -- Stored response envelope so retries return the original 201 body.
    -- `response_body` is a serialized JSON string (text), not jsonb — it is
    -- deserialized back into the response DTO on replay, never queried.
    response_status int         NOT NULL,
    response_body   text        NOT NULL,
    response_etag   text        NOT NULL,

    -- The authenticated subject that created this key.
    -- Not part of the PK (the composite key below is unchanged); the domain
    -- layer fetches by the composite key and then verifies
    -- `record.subject_id == ctx.subject_id()`, treating a mismatch as
    -- Forbidden rather than silently falling through to a fresh create.
    -- Pre-migration rows are backfilled with the nil UUID, which can never
    -- match a real subject.
    subject_id     uuid         NOT NULL  DEFAULT '00000000-0000-0000-0000-000000000000',

    -- SHA-256 over a canonicalized, length-prefixed
    -- encoding of the identity-relevant request fields (owner_kind, owner_id,
    -- name, gts_file_type, mime_type, custom_metadata) at insert time. A
    -- replay recomputes this hash from the current request and rejects a
    -- mismatch with 409 Conflict
    -- ("idempotency key reused with a different request body"), instead of
    -- silently replaying the original ticket. Pre-migration rows default to
    -- an empty blob, which can never match a freshly computed digest.
    request_hash   bytea        NOT NULL  DEFAULT '\x',

    created_at     timestamptz  NOT NULL  DEFAULT now(),
    expires_at     timestamptz  NOT NULL,

    PRIMARY KEY (tenant_id, owner_kind, owner_id, idempotency_key)
);

CREATE INDEX idempotency_keys_expired_idx ON file_storage.idempotency_keys (expires_at);


-- Table: file_storage.audit_outbox -------------------------------------------
-- Transactional outbox for the audit-publisher. Rows are inserted in the
-- same DB transaction as the writes they describe, then drained by a worker
-- and forwarded to the platform audit sink. Provides 100% coverage with no
-- silent drops (NFR cpt-cf-file-storage-nfr-audit-completeness).

CREATE TABLE file_storage.audit_outbox (
    event_id        uuid         PRIMARY KEY  DEFAULT gen_random_uuid(),
    tenant_id       uuid         NOT NULL,
    actor_kind      text         NOT NULL,
    actor_id        uuid         NOT NULL,
    file_id         uuid,
    operation       text         NOT NULL,        -- 'create' | 'patch_content' | 'patch_metadata' | 'delete_file' |
                                                   -- 'delete_version' | 'multipart_complete' | 'multipart_abort' |
                                                   -- 'finalize_version' | 'retention_delete' | 'backend_migrate' |
                                                   -- 'orphan_reconcile' | 'transfer_ownership'
                                                   -- (exhaustive: see `AuditOperation::as_str`, src/domain/audit.rs)
    outcome         text         NOT NULL,        -- 'success' | 'failure'
    detail          jsonb        NOT NULL,        -- arbitrary structured detail
    occurred_at     timestamptz  NOT NULL  DEFAULT now(),
    published_at    timestamptz                   -- NULL until drained
);

CREATE INDEX audit_outbox_unpublished_idx
    ON file_storage.audit_outbox (occurred_at)
    WHERE published_at IS NULL;


-- Table: file_storage.events_outbox ------------------------------------------
-- Outbox for EventBroker file-event publication. Same pattern as audit_outbox
-- but targets the platform EventBroker, per cpt-cf-file-storage-fr-file-events.
--
-- NOT YET ENFORCED (tracked gap): PRD.md's fr-file-events requires owner
-- policy to define which event types are enabled
-- (`PolicyBody.enabled_event_types`, src/domain/policy.rs), and that field is
-- stored/round-tripped through GET/PUT /policy. But no enqueue path
-- currently consults it — every event type below is enqueued unconditionally
-- on its triggering mutation. "Policy-gated" describes the intended design,
-- not the shipped behavior.

CREATE TABLE file_storage.events_outbox (
    event_id        uuid         PRIMARY KEY  DEFAULT gen_random_uuid(),
    tenant_id       uuid         NOT NULL,
    owner_id        uuid         NOT NULL,
    file_id         uuid         NOT NULL,
    event_type      text         NOT NULL,        -- 'file.created' | 'file.content_updated' |
                                                   -- 'file.metadata_updated' | 'file.owner_transferred' |
                                                   -- 'file.deleted'
                                                   -- (exhaustive: see every `make_file_event`/`FileEvent{...}`
                                                   -- call site under src/domain/service/ and src/domain/cleanup.rs)
    payload         jsonb        NOT NULL,
    occurred_at     timestamptz  NOT NULL  DEFAULT now(),
    published_at    timestamptz
);

CREATE INDEX events_outbox_unpublished_idx
    ON file_storage.events_outbox (occurred_at)
    WHERE published_at IS NULL;


-- Table: file_storage.policies -----------------------------------------------
-- Tenant and user policies (allowed types, size limits, retention and
-- lifecycle controls). Effective policy is the most
-- restrictive across applicable rows (per PRD §5.4).

CREATE TABLE file_storage.policies (
    policy_id        uuid         PRIMARY KEY  DEFAULT gen_random_uuid(),
    tenant_id        uuid         NOT NULL,
    -- Scope of the policy. user-level policies match against the file's
    -- owner_id when owner_kind = 'user'; tenant-level policies match
    -- against the file's tenant.
    scope            text         NOT NULL  CHECK (scope IN ('tenant', 'user')),
    scope_owner_id   uuid,                       -- NULL when scope='tenant'

    -- Policy body. Structure documented in P2 FEATURE artifacts.
    body             jsonb        NOT NULL,

    created_at       timestamptz  NOT NULL  DEFAULT now(),
    updated_at       timestamptz  NOT NULL  DEFAULT now(),

    CHECK ((scope = 'user' AND scope_owner_id IS NOT NULL) OR
           (scope = 'tenant' AND scope_owner_id IS NULL))
);

CREATE INDEX policies_scope_idx
    ON file_storage.policies (tenant_id, scope, scope_owner_id);

-- Two partial unique indexes (not one plain composite
-- unique index) close the upsert delete-then-insert race. Postgres (and
-- SQLite) treat every NULL as distinct for uniqueness, so a single
-- `UNIQUE (tenant_id, scope, scope_owner_id)` index would dedupe user-scope
-- rows but silently allow unlimited tenant-scope rows (scope_owner_id IS
-- NULL) for the same tenant. `PolicyRepo::upsert`'s delete+insert pair is
-- also wrapped in an explicit DB transaction; these indexes are the backstop
-- for the remaining no-existing-row race (two concurrent first-time upserts).
CREATE UNIQUE INDEX policies_user_scope_unique_idx
    ON file_storage.policies (tenant_id, scope, scope_owner_id)
    WHERE scope_owner_id IS NOT NULL;

CREATE UNIQUE INDEX policies_tenant_scope_unique_idx
    ON file_storage.policies (tenant_id, scope)
    WHERE scope_owner_id IS NULL;


-- Table: file_storage.retention_rules ----------------------------------------
-- Tenant/user retention rules. Background worker evaluates against
-- file metadata and deletes when criteria are met.

CREATE TABLE file_storage.retention_rules (
    rule_id          uuid         PRIMARY KEY  DEFAULT gen_random_uuid(),
    tenant_id        uuid         NOT NULL,
    scope            text         NOT NULL  CHECK (scope IN ('tenant', 'user', 'file')),
    scope_target_id  uuid,                       -- user_id when scope='user'; file_id when scope='file'; NULL when scope='tenant'

    -- Rule body: age-based, inactivity-based, custom-metadata-based.
    body             jsonb        NOT NULL,

    created_at       timestamptz  NOT NULL  DEFAULT now(),

    CHECK ((scope = 'tenant' AND scope_target_id IS NULL) OR
           (scope IN ('user', 'file') AND scope_target_id IS NOT NULL))
);

CREATE INDEX retention_rules_scope_idx
    ON file_storage.retention_rules (tenant_id, scope, scope_target_id);

CREATE INDEX retention_rules_file_scope_idx
    ON file_storage.retention_rules (scope_target_id)
    WHERE scope = 'file';


-- =============================================================================
-- P3 — Runtime Backend Configuration, Encryption metadata
-- =============================================================================

-- Table: file_storage.storage_backends_runtime ------------------------------
-- DB-resident replacement for the P1 TOML configuration file. When this
-- table is populated, the BackendRegistry switches its source from TOML to
-- DB on gear startup. Credentials are stored encrypted at rest; the
-- envelope encryption is managed by the platform secret store
-- (PRD `cpt-cf-file-storage-fr-runtime-backends`).

CREATE TABLE file_storage.storage_backends_runtime (
    backend_id       text         PRIMARY KEY,
    kind             text         NOT NULL,         -- 'local-filesystem' | 's3-compatible' | ...
    endpoint         text,                          -- nullable for local-filesystem
    region           text,                          -- nullable for non-cloud backends

    -- Credentials encrypted via the platform secret store. The column is
    -- an opaque blob; FileStorage never reads or writes the plaintext
    -- credentials directly — the secret-store SDK does that on every load.
    credentials_blob bytea,
    credentials_kms_key_id text,

    -- Capabilities (multipart_native, encryption_native, range_native;
    -- no versioning_native — versioning is FileStorage-level) serialized as
    -- JSON. Loaded into BackendCapabilities
    -- struct at registry build time.
    capabilities     jsonb        NOT NULL,
    hash_policy      jsonb        NOT NULL,        -- HashPolicy (default_algorithm, allowed_algorithms, selection_rules)

    -- Soft-disable without removing the row (e.g., during scheduled
    -- maintenance). When false, the registry skips this backend; pre-existing
    -- file rows pointing at it return 503 on content access.
    enabled          boolean      NOT NULL  DEFAULT true,

    created_at       timestamptz  NOT NULL  DEFAULT now(),
    updated_at       timestamptz  NOT NULL  DEFAULT now()
);

CREATE INDEX storage_backends_runtime_enabled_idx
    ON file_storage.storage_backends_runtime (enabled)
    WHERE enabled = true;


-- P3 version-row extensions for encryption -----------------------------------
-- Per-version encryption metadata for server-side encryption with backend-managed
-- or customer-provided keys. Lives on file_versions (encryption is a property of
-- the stored object). Populated only when the writing backend declares
-- encryption_native = true and the operative policy enables encryption.

ALTER TABLE file_storage.file_versions
    ADD COLUMN encryption_scheme  text,
    ADD COLUMN encryption_kms_key_id text,
    ADD COLUMN encryption_metadata jsonb;

COMMENT ON COLUMN file_storage.file_versions.encryption_scheme IS
    'P3: name of the server-side encryption scheme applied (e.g., AES256-GCM-SSE-S3, AES256-GCM-SSE-KMS). NULL when the backend did not encrypt.';
COMMENT ON COLUMN file_storage.file_versions.encryption_kms_key_id IS
    'P3: key identifier in the platform KMS / secret store, when SSE-KMS is used.';
