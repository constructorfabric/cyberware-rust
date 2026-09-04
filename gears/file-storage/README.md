# FileStorage

Universal file storage and management service for the Gears middleware.

## Overview

FileStorage provides upload, download, metadata management, access control, and sharing capabilities for all platform
gears and users. It replaces ad-hoc per-gear file handling with a centralized, tenant-aware storage service.

### Key Capabilities

- **File operations** — upload, download, delete, list with rich metadata
- **Pluggable backends** — S3, GCS, Azure Blob, NFS, FTP, SMB, WebDAV, local filesystem
- **Access control** — tenant-scoped ownership, GTS file type classification, Authorization Service integration
- **Sharing** — shareable links (public/tenant/hierarchy scopes), signed URLs, direct transfer URLs
- **Access interfaces** — REST API, S3-compatible API, WebDAV API
- **Policies** — file type restrictions, size limits, sharing model restrictions, storage quotas
- **Lifecycle** — file versioning, retention policies, multipart upload, conditional requests (ETags)
- **Audit** — write operation audit trail, optional read audit logging

### Actors

| Actor               | Description                                                                   |
|---------------------|-------------------------------------------------------------------------------|
| Platform User       | Authenticated user managing files via UI or API                               |
| CF/Gears | Any gear requiring file operations (e.g., LLM Gateway, document management) |

### Dependencies

| Dependency            | Criticality |
|-----------------------|-------------|
| ToolKit Framework      | p1          |
| Authorization Service | p1          |
| Audit Infrastructure  | p2          |
| Usage Collector       | p2          |
| Quota Enforcement     | p2          |
| EventBroker           | p2          |
| Serverless Runtime    | p2          |

## Documentation

- [PRD.md](docs/PRD.md) — Product requirements document
- [DESIGN.md](docs/DESIGN.md) — Architecture and design
- [DECOMPOSITION.md](docs/DECOMPOSITION.md) — Feature decomposition strategy
- [api.md](docs/api.md) — HTTP API reference
- [ADR/](docs/ADR/) — Architecture decision records
- [features/](docs/features/) — Per-feature specs (multipart coordinator, …)

## Implementation status

### Control plane and sidecar

FileStorage's control plane and data-plane sidecar are implemented and tested. Highlights:

- Two crates: `cf-gears-file-storage-sdk` (public API) + `cf-gears-file-storage` (gear lib + `sidecar` binary).
- Control-plane REST under `/api/file-storage/v1` (create/presign/bind, download-URL, metadata CRUD, list,
  versions, storages) — JSON only; content never transits the control plane.
- Immutable-blob + content-pointer model with optimistic-CAS bind, FileStorage-level versioning, tenant isolation,
  Authorization-Service per-type checks, conditional requests (ETag / `If-Match` / `If-None-Match`).
- Pluggable backends (trait + `local-fs` + `in-memory`); Ed25519 signed URLs (codec-equivalent to, but not literal,
  PASETO `v4.public` — see [ADR-0004](docs/ADR/0004-cpt-cf-file-storage-adr-signed-url-transport.md)'s
  Implementation note); SHA-256 + magic-byte content-type validation; HTTP `Range`. Data-plane **sidecar** binary
  verifies tokens and streams bytes, then calls a token-authenticated `finalize` callback back to the control plane
  (`pending → available`); binding a version as the file's live content (`content_id`) is always a separate,
  client-issued request (see [DESIGN.md](docs/DESIGN.md) §3.6 and
  [ADR-0003](docs/ADR/0003-cpt-cf-file-storage-adr-sidecar-data-plane.md)).

### Policies, lifecycle, and governance

Built on top of the control plane and sidecar above, FileStorage also provides:

- **Policy engine** — allowed-types / size / custom-metadata-limit policies, resolved at tenant and user scope
  (`GET`/`PUT /policy`, `GET /policy/effective`). See
  [docs/features/policy-engine.md](docs/features/policy-engine.md).
- **Retention rules + background cleanup sweep** — per-tenant retention rules (`/retention-rules`) plus a background
  process that prunes expired files and reconciles orphaned backend objects. See
  [docs/features/retention-cleanup.md](docs/features/retention-cleanup.md).
- **Idempotent create** — `POST /files` is safe to retry.
- **Audit outbox** — a transactional outbox recording write operations (create, finalize, bind, metadata update,
  delete, ownership transfer, backend migration, …) for downstream audit consumption. The write side is implemented
  and tested; **draining and relaying those records to a downstream consumer is not implemented** — see
  [docs/features/audit-trail.md](docs/features/audit-trail.md).
- **Events outbox** — file lifecycle events (`file.created`, `file.content_updated`, `file.metadata_updated`,
  `file.owner_transferred`, `file.deleted`) are written transactionally alongside the mutation. They are **not
  drained** to the platform EventBroker (the same undrained-relay characteristic as the audit outbox above). The
  `enabled_event_types` policy knob **is stored but not enforced** — `PolicyBody` round-trips an
  `enabled_event_types` list through `GET`/`PUT /policy`, but no enqueue path consults it: every event type above is
  always enqueued, regardless of policy. See [docs/features/policy-engine.md](docs/features/policy-engine.md) for
  detail.
- **Ownership transfer** — `POST /files/{id}/transfer`, atomic owner swap with audit + event + usage-delta reporting.
  Target-owner validation is **partial** — only the nil-UUID sentinel is rejected; see
  [docs/features/ownership-transfer.md](docs/features/ownership-transfer.md).
- **Backend migration** — `POST /files/{id}/migrate`, relocates a non-versioned file's content to a different backend
  with a verified, mode-aware content-hash check before committing. See
  [docs/features/backend-migration.md](docs/features/backend-migration.md).
- **Multipart upload** — the control plane computes a server-authoritative parts plan and mints a per-part signed
  URL for each; the sidecar's report-part callback records each part's hash. `complete` returns `200` with the
  version id, size, composite hash, and manifest, accepts an optional `If-Match`, and returns `409` with the list of
  missing parts when the upload is incomplete; `GET .../multipart/{upload_id}` introspects an in-progress upload and
  reissues signed URLs for missing parts (resume); abort deletes the part rows and the pending version.
  **Functional only against a `multipart_native` backend** (today: the non-durable in-memory backend for dev/test,
  and configured S3 backends) — the default `local-fs` backend does not declare `multipart_native`, so
  `POST /files/{id}/multipart` is rejected against the default topology. S3 remains opt-in and gated by the
  ADR-0005 external-dependency security review. See
  [docs/features/multipart-coordinator.md](docs/features/multipart-coordinator.md) for the full contract, including
  the part-hash trust note below.
- **Storage quota is not enforced.** `check_quota`/`check_quota_bytes` gate every storage-increasing operation
  (`create_file`, `presign_version`, multipart initiate) via the `QuotaClient` port
  (`src/infra/external_clients.rs`), and are designed to fail **closed** once a real client is wired — a client
  error is propagated and denies the request (see `tests/enforce_test.rs`). `gear.rs` constructs both services with
  `quota_client: None`, and `None` makes the check a no-op (`Ok(())`), so **no deployment enforces storage quota
  today**; the effective default is permissive (fail-**open**), the opposite of the port's fail-closed design. This
  is blocked on a Quota Enforcement SDK: `gears/system/quota-enforcement/` is docs-only (PRD/DESIGN/ADRs, no Rust
  crate) — there is no real client to wire in. Usage reporting is further along: a `usage-collector-sdk` crate
  exists, though `usage_reporter` is likewise wired as `None` pending its own integration.
- **Multipart uploads trust the caller-reported per-part hash.** Single-shot uploads re-derive
  size/hash/MIME from a real backend read-back at finalize time, so a forged claim cannot corrupt
  stored metadata. Multipart uploads have no equivalent: `report_part` persists the caller-supplied
  per-part hash after only a length/size check, and `complete` builds the composite hash from those
  stored hashes with no re-read of the assembled object. See
  [ADR-0003](docs/ADR/0003-cpt-cf-file-storage-adr-sidecar-data-plane.md)'s "Known gap" note for the
  mitigation available today (`require_finalize_internal_secret`).

**Not yet implemented**: sharing (shareable links) and WebDAV. Storage-quota enforcement is likewise not wired (see
above). The S3 backend is implemented and available as an opt-in backend for the control plane and sidecar, gated
by the ADR-0005 external-dependency security review before merge or release use.

### Run

```bash
cargo build -p cf-gears-file-storage                 # control-plane gear (lib)
cargo build -p cf-gears-file-storage --bin sidecar   # data-plane sidecar
cargo test  -p cf-gears-file-storage -p cf-gears-file-storage-sdk

# Sidecar env (4 of 9 — see docs/operations.md for the rest): FS_SIDECAR_ADDR,
# FS_SIDECAR_PUBLIC_KEY (base64url Ed25519), FS_SIDECAR_BACKEND_ROOT,
# FS_SIDECAR_CONTROL_URL (control-plane base URL for the finalize/report-part
# callbacks -- without it, uploads default to http://localhost:8080 and stay
# `pending` forever against any other control-plane address)
```
