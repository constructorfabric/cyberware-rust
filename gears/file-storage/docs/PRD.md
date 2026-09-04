# PRD — File Storage


<!-- toc -->

- [1. Overview](#1-overview)
  - [1.1 Purpose](#11-purpose)
  - [1.2 Background / Problem Statement](#12-background--problem-statement)
  - [1.3 Goals (Business Outcomes)](#13-goals-business-outcomes)
  - [1.4 Success Metrics](#14-success-metrics)
  - [1.5 Glossary](#15-glossary)
- [2. Actors](#2-actors)
  - [2.1 Human Actors](#21-human-actors)
  - [2.2 System Actors](#22-system-actors)
- [3. Operational Concept & Environment](#3-operational-concept--environment)
  - [3.1 Gear-Specific Environment Constraints](#31-gear-specific-environment-constraints)
- [4. Scope](#4-scope)
  - [4.1 In Scope](#41-in-scope)
  - [4.2 Out of Scope](#42-out-of-scope)
- [5. Functional Requirements](#5-functional-requirements)
  - [5.1 Core File Operations](#51-core-file-operations)
  - [5.2 Ownership & Access Control](#52-ownership--access-control)
  - [5.3 Sharing](#53-sharing)
  - [5.4 Policies (Phase 2)](#54-policies-phase-2)
  - [5.5 Metadata](#55-metadata)
  - [5.6 File Retention & Lifecycle](#56-file-retention--lifecycle)
  - [5.7 Audit](#57-audit)
  - [5.8 Pluggable Storage Backends](#58-pluggable-storage-backends)
  - [5.9 Access Interfaces](#59-access-interfaces)
  - [5.10 Cache & Idempotency](#510-cache--idempotency)
- [6. Non-Functional Requirements](#6-non-functional-requirements)
  - [6.1 Gear-Specific NFRs](#61-gear-specific-nfrs)
  - [6.2 NFR Exclusions](#62-nfr-exclusions)
  - [6.3 Applicability Notes](#63-applicability-notes)
- [7. Public Library Interfaces](#7-public-library-interfaces)
  - [7.1 Public API Surface](#71-public-api-surface)
  - [7.2 External Integration Contracts](#72-external-integration-contracts)
- [8. Use Cases](#8-use-cases)
  - [Upload a File](#upload-a-file)
  - [Fetch File for Gear Processing](#fetch-file-for-gear-processing)
  - [Validate File Metadata Before Processing](#validate-file-metadata-before-processing)
  - [Delete a File](#delete-a-file)
  - [Multi-Backend Deployment](#multi-backend-deployment)
  - [Configure Policy](#configure-policy)
- [9. Acceptance Criteria](#9-acceptance-criteria)
- [10. Dependencies](#10-dependencies)
- [11. Assumptions](#11-assumptions)
- [12. Risks](#12-risks)
- [13. Open Questions](#13-open-questions)
- [14. Traceability](#14-traceability)

<!-- /toc -->

## 1. Overview

### 1.1 Purpose

FileStorage is a universal file storage and management service for the Gears middleware. It provides upload,
download, metadata management, and tenant-scoped access control for any gear or user within the platform. All
access in P1 is authenticated — anonymous/external sharing is deferred to a separate concern (P3, see `§5.3`).

FileStorage is split into two cooperating planes (see
[ADR-0003](./ADR/0003-cpt-cf-file-storage-adr-sidecar-data-plane.md)):

- a **control plane** (the FileStorage API/SDK) that owns metadata, authorization, versioning, and
  conditional-request semantics, and whose REST surface **never carries file content** — it issues short-lived
  **signed URLs** instead;
- a **data-plane sidecar** that is the only component to move user bytes, is connected to the storage backends, and
  serves content exclusively through those signed URLs.

The sidecar is a deliberate **platform-level exception** to the standard "API Gateway owns REST hosting" model: it is
**not** an ordinary gear REST route fronted by the API Gateway, but a dedicated byte-moving data plane with its own
host/URL that clients reach directly via signed URLs. This exception is what lets the data plane scale independently
and keeps content off the control-plane (gateway) path; how it authenticates and is contracted is specified in
DESIGN.md §3.3 and §4.5.

Consequently every content operation is at least two requests: a control request to obtain a signed URL, plus one or
more data requests against the sidecar. Backends are never addressed by clients directly — the signed URL always
points at the sidecar — so backend opacity, centralized per-byte metering, and uniform audit/policy coverage are
preserved while the byte-moving data plane scales independently of the control plane.

The service supports pluggable storage backends, tenant-scoped access control with an ownership model, and
policy-driven governance for file types and sizes.

### 1.2 Background / Problem Statement

Gears and platform users require file storage for various purposes: gears handle multimodal AI content
(images, audio, video, documents), documents and artifacts, reporting outputs, and platform users need direct file
access through standard protocols.

Without a dedicated storage service, each gear implements ad-hoc file handling, media gets inlined as base64 in API
payloads (bloating requests and hitting size limits), provider-generated URLs expire leaving consumers with broken
links, and there is no unified access control or policy enforcement across the platform.

FileStorage solves this by providing a centralized, tenant-aware storage service with persistent URLs, pluggable
backends, and standardized access interfaces — functioning as a superset of S3 and WebDAV capabilities within the
Gears security and governance model.

### 1.3 Goals (Business Outcomes)

- Unified file storage accessible by all Gears and platform users
- Tenant-scoped and origin-gear-scoped access control with tenant, user and gear ownership model
- Policy-driven governance over file types, sizes, and events
- Audit trail for all write operations
- Pluggable storage backends without service rebuild

### 1.4 Success Metrics

| Metric                                   | Baseline                                 | Target                                                           | Timeframe                      |
|------------------------------------------|------------------------------------------|------------------------------------------------------------------|--------------------------------|
| Gear adoption rate                     | 0% (ad-hoc file handling)                | 90%+ of file-dependent gears use FileStorage SDK               | 6 months after GA              |
| Base64-inlined media payloads            | Present in LLM Gateway and other gears | 0 base64 file payloads in gears that adopted FileStorage       | 3 months after gear adoption |
| Broken/expired provider URLs             | Recurring in downstream workflows        | 0 broken URLs for files within retention period                  | Ongoing after GA               |
| Audit coverage for file write operations | No centralized audit                     | 100% of write operations audited                                 | Phase 2                        |
| Multi-backend deployment                 | Single ad-hoc storage per gear         | At least 2 backend types validated (e.g., S3 + local filesystem) | At GA                          |

### 1.5 Glossary

| Term                | Definition                                                                                                                                                                                                                                                                              |
|---------------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| File                | Binary content stored in FileStorage with associated metadata                                                                                                                                                                                                                           |
| Control Plane       | The FileStorage API/SDK. Owns metadata, authorization, versioning, and conditional-request semantics; issues signed URLs. Its REST surface never carries file content                                                                                                                    |
| Sidecar (Data Plane)| The only component that moves user bytes. Has its own domain/URL, is connected to the storage backends, validates platform auth tokens and signed-URL signatures, and reaches the control plane via the FS SDK. Serves content only through signed URLs                                  |
| Signed URL          | A short-lived, control-minted **PASETO `v4.public`** token (Ed25519) pointing at the sidecar that authorizes one content operation (`GET`/`PUT`/part) on a specific object, subject to AND-combined claims (`exp`, optional `ip`, optional token-claim predicates, upload size/hash). Carried in the query (`?fs-token=`) or a header; **opaque** to all but control+sidecar (`cpt-cf-file-storage-fr-signed-urls`) |
| File ID             | The immutable uuid identity of a logical file. The current content is reached by resolving the file's content pointer (`content_id`)                                                                                                                                                     |
| Version ID          | A uuid assigned by FileStorage (control plane) identifying one immutable content blob; the backend object lives at `/{file_id}/{version_id}` and is never mutated in place                                                                                                                |
| Content Pointer (`content_id`) | The `version_id` currently bound as a file's live content; changing content is a pointer swap, not an in-place mutation. The content-only ETag derives from `(file_id, content_id)`                                                                                            |
| Metadata Revision (`meta_version`) | A monotonic counter bumped on metadata-only writes; the validator for `If-Match-Metadata`                                                                                                                                                                          |
| Bind                | The control-plane operation that swaps a file's `content_id` to a (`pending` → `available`) version under optimistic CAS (`If-Match`). A conflict (`400 failed_precondition`) is retried by re-binding the already-uploaded `version_id` — never by re-uploading bytes                                       |
| Metadata            | File properties: system-managed (name, size, mime_type, GTS file type, dates, owner) and user-defined custom key-value pairs                                                                                                                                                            |
| Custom Metadata     | User-defined key-value pairs attached to a file, analogous to S3 object metadata                                                                                                                                                                                                        |
| Owner               | The principal that owns a file: `owner_kind ∈ {user, app}` plus `owner_id`. Every file also has a separate immutable `tenant_id`                                                                                                                                                       |
| FileShare           | Working name for the future (P3) sharing capability built on top of FileStorage. Covers anonymous/public URLs, per-recipient grants, expirations, download counters, etc. Whether it ships as a separate Gear or as an extension of FileStorage is deferred to a future ADR  |
| Sharable Link       | A FileShare-issued (P3) reference to a FileStorage file with optional content/version pinning and access rules (anonymity, expiration, recipients, maximum download count). Out of P1 scope                                                                                                |
| Storage Backend     | An underlying storage system (S3, GCS, Azure Blob, NFS, FTP, SMB, WebDAV) used for persisting file content                                                                                                                                                                              |
| Policy              | A set of rules (allowed file types, size limits, events, sharing models) that constrain file operations; applicable at the tenant level and the user level independently — when both apply, the most restrictive value per aspect wins                                                  |
| File Version        | An immutable content blob created on each content write, stored as a distinct backend object `/{file_id}/{version_id}`. FileStorage-level (not backend-native), so versioning works on any backend (`cpt-cf-file-storage-fr-file-versioning`)                                            |
| File Type (GTS)     | A GTS type identifier assigned to every file at upload time that classifies the file by domain, actor, and purpose (e.g., `gts.cf.fstorage.file.type.v1~x.genai.llm.autogenerated.v1~`); used by the Authorization Service to enforce per-type access control between actors and gears |
| Backend Capability  | An optional feature that a storage backend may or may not support (e.g., presigned URLs, versioning, multipart upload); FileStorage discovers available capabilities per backend and adapts its behavior accordingly                                                                    |

## 2. Actors

### 2.1 Human Actors

#### Platform User

**ID**: `cpt-cf-file-storage-actor-platform-user`

**Role**: Authenticated user who uploads, downloads, and manages files through the platform UI or API.
**Needs**: Direct file access, sharing capabilities, metadata management, and self-service link management.

### 2.2 System Actors

#### Gears

**ID**: `cpt-cf-file-storage-actor-cf-gears`

**Role**: Any Gear requiring file upload, download, metadata retrieval, or link management (e.g., LLM
Gateway for multimodal media, document management gears, reporting gears).

## 3. Operational Concept & Environment

### 3.1 Gear-Specific Environment Constraints

FileStorage operates within the standard Gears runtime environment. Authentication and identity management are
fully delegated to the platform — FileStorage does not implement its own authentication layer. All incoming requests are
pre-authenticated by the platform infrastructure, and FileStorage receives the caller's identity context (user, tenant,
roles) from the platform authentication middleware.

## 4. Scope

### 4.1 In Scope

- Two-plane architecture: a control plane (API/SDK, metadata + authorization) and a data-plane sidecar (byte
  transfer), per [ADR-0003](./ADR/0003-cpt-cf-file-storage-adr-sidecar-data-plane.md)
- Signed-URL content access: the control plane issues short-lived signed URLs that authorize a single content
  operation against the sidecar (constraints: expiry, optional ip, optional token-claim predicates)
- Immutable-blob + content-pointer model with FileStorage-level versioning (backend-agnostic)
- Upload, download, delete, and list files
- Rich file metadata storage, retrieval, and update
- File ownership by user or app (Gear) within a tenant
- GTS file type classification for per-actor access control
- Authorization checks via Authorization Service
- Audit trail for all write operations and optional read audit logging
- Policies (file types, size limits, events) at tenant and user levels
- Pluggable storage backend abstraction
- Backend migration — relocating a file's content between backends without rotating its URL (P2; non-versioned files)
- Multipart (chunked) upload for large files
- Content-type validation against actual file content
- File retention and lifecycle management
- REST API access interface
- Random read access via HTTP Range requests
- Static (P1) and runtime (P3) storage backend configuration
- Storage quota enforcement via Quota Enforcement service
- Ownership transfer
- Custom metadata limits
- File versioning
- Conditional requests (ETags) for cache validation and concurrent update protection
- Upload idempotency
- Owner deletion handling via EventBroker and Serverless Runtime workflows
- File encryption (server-side, per backend capability and configuration)

### 4.2 Out of Scope

- Content transformation or transcoding
- CDN distribution
- Full-text search within file content
- All external/anonymous access (anonymous URLs, scope-based shareable links, per-recipient grants, time-bounded
  or count-limited access) — deferred to P3 (see `§5.3`). FileStorage P1 exposes only the auth-required surface
- S3-compatible and WebDAV protocol facades

## 5. Functional Requirements

### 5.1 Core File Operations

#### Upload File

- [ ] `p1` - **ID**: `cpt-cf-file-storage-fr-upload-file`

The system **MUST** accept file content with metadata and persist it. Upload is a two-step exchange: the client first
asks the control plane for a **signed upload URL** (`cpt-cf-file-storage-fr-signed-urls`), then transfers the bytes to
the **sidecar** at that URL; the control-plane REST surface never receives the content itself. Each upload writes a
new **immutable** content blob to the backend object `/{file_id}/{version_id}`; the file's live content is a pointer
(`content_id`) that is **bound** to that version (`cpt-cf-file-storage-fr-file-versioning`). Backend content is
**never mutated in place** — replacing content writes a new version and swaps the pointer; partial-byte mutation is
**not** supported.

Binding the pointer is an optimistic compare-and-swap guarded by `If-Match` (`cpt-cf-file-storage-fr-conditional-requests`):
if the file's content changed concurrently the bind returns `400 failed_precondition`, and the client **MUST** be able to retry the bind
against the already-uploaded `version_id` **without re-uploading the bytes**. Rebinding is a **control-plane** operation
independent of the signed upload URL — the upload URL's expiry does **not** affect the retry and the client does **not**
re-presign; the failed-precondition response returns the current content ETag for the fresh `If-Match`. (Re-presigning instead would upload a
new sibling version, later reconciled by `cpt-cf-file-storage-fr-orphan-reconciliation`.)

**Rationale**: All platform gears and users need to store files — gears store generated content, documents, and
artifacts, users upload files directly. Separating the signed control request from the sidecar byte transfer keeps the
control plane out of the data path; the immutable-blob + pointer model makes versioning backend-agnostic and makes a
concurrent-write conflict cheap to recover from (re-bind, never re-upload).
**Actors**: `cpt-cf-file-storage-actor-platform-user`, `cpt-cf-file-storage-actor-cf-gears`

#### Download File

- [ ] `p1` - **ID**: `cpt-cf-file-storage-fr-download-file`

The system **MUST** retrieve file content for consumption by requesting actors via a two-step exchange: the client
asks the control plane for a **signed download URL** (`cpt-cf-file-storage-fr-signed-urls`), then fetches the bytes
from the **sidecar** at that URL. The signed URL pins a specific `content_id` (an immutable version), so it is stable
and cacheable; the sidecar serves the bytes, honours `Range` (`cpt-cf-file-storage-fr-range-requests`) and conditional
requests, and emits the response headers carried in the signed URL. The control-plane REST surface never returns the
content itself. File **metadata** is retrieved separately and directly from the control plane
(`cpt-cf-file-storage-fr-get-metadata`).

**Rationale**: All platform gears and users need to retrieve stored files — gears fetch media and documents, users
download files directly. Issuing a signed URL keeps the control plane out of the byte path while preserving backend
opacity (the URL points at the sidecar, never the backend) and central metering/audit on the sidecar.
**Actors**: `cpt-cf-file-storage-actor-platform-user`, `cpt-cf-file-storage-actor-cf-gears`

#### Delete File

- [ ] `p1` - **ID**: `cpt-cf-file-storage-fr-delete-file`

The system **MUST** allow any actor authorized for the **delete** action on the file's GTS type
(`cpt-cf-file-storage-fr-authorization`) to delete a file. Deleting a file removes its metadata, ownership records, and
**all** of its versions; the backend objects are deleted best-effort (a failed backend delete degrades to an orphan
reconciled by the P2 cleanup engine, `cpt-cf-file-storage-fr-orphan-reconciliation`). Deletion is **idempotent** —
re-deleting an already-deleted file returns `404`. A single version may instead be removed by `version_id`
(`cpt-cf-file-storage-fr-file-versioning`), deleting only that version; deleting the only remaining version is
equivalent to deleting the file.

**Rationale**: Authorized actors need to remove files that are no longer needed. Recovery from an accidental content
overwrite is provided by versioning/restore within the file's lifetime (`cpt-cf-file-storage-fr-file-versioning`); a
file-level delete is intentional and removes the whole file. The metadata row is removed before the best-effort
backend delete, so a deleted file never leaves a row pointing at missing bytes.
**Actors**: `cpt-cf-file-storage-actor-platform-user`, `cpt-cf-file-storage-actor-cf-gears`

#### Get File Metadata

- [ ] `p1` - **ID**: `cpt-cf-file-storage-fr-get-metadata`

The system **MUST** return file metadata (name, size, mime_type, GTS file type, created date, modified date, owner,
and custom metadata) without transferring file content.

**Rationale**: Consumers validate file properties (size limits, type compatibility) and read custom metadata before
initiating downloads, avoiding wasted bandwidth on incompatible files.
**Actors**: `cpt-cf-file-storage-actor-platform-user`, `cpt-cf-file-storage-actor-cf-gears`

#### List Files

- [ ] `p1` - **ID**: `cpt-cf-file-storage-fr-list-files`

The system **MUST** support listing files with their metadata (no content transfer). The caller **MUST** specify the
owner type as a mandatory filter:

- **User-owned** — files owned by a specific user (`owner_kind = user`)
- **App-owned** — files owned by a Gear (`owner_kind = app`)

The response **MUST** be paginated following the platform API guidelines (cursor-based or offset-based pagination with
configurable page size). The system **MUST** support optional additional filters (mime_type, date range, custom metadata
keys).

**Rationale**: Users and gears need to discover and browse files they own or have access to. Mandatory owner type
filtering prevents unbounded queries across all files and aligns with the ownership model.
**Actors**: `cpt-cf-file-storage-actor-platform-user`, `cpt-cf-file-storage-actor-cf-gears`

#### Multipart Upload

- [ ] `p2` - **ID**: `cpt-cf-file-storage-fr-multipart-upload`

The system **MUST** support multipart (chunked) upload for large files. Multipart upload requires the multipart
upload backend capability (`cpt-cf-file-storage-fr-backend-capabilities`). A multipart upload **MUST**:

- Allow the client to split a file into multiple parts and upload them independently
- Support resumable uploads — if a part fails, only that part needs re-uploading
- Assemble parts into a complete file upon finalization
- Apply the same authorization, metadata, and audit requirements as single-part uploads

For backends that do not declare the multipart upload capability, the system **MUST** reject multipart upload requests
with a clear error indicating the capability is unavailable. There is no FileStorage-level fallback for multipart —
clients must use single-part upload for backends without native multipart support.

**Rationale**: Single-request uploads are impractical for large files (video, datasets, backups) due to timeouts,
memory constraints, and network reliability. Multipart upload enables reliable transfer of arbitrarily large files.
Implementing multipart at the FileStorage layer without backend support would require full content buffering, negating
the scalability benefits. Rejecting with a clear error lets clients adapt their upload strategy per backend.
**Actors**: `cpt-cf-file-storage-actor-platform-user`, `cpt-cf-file-storage-actor-cf-gears`

#### Content-Type Validation

- [ ] `p1` - **ID**: `cpt-cf-file-storage-fr-content-type-validation`

The system **MUST** validate the declared mime_type against the actual file content (magic bytes / file signature) on
every upload (all upload traffic transits the sidecar). If the declared type does not match the detected type, the
system **MUST** reject the upload with an error indicating the mismatch.

For multipart uploads (`cpt-cf-file-storage-fr-multipart-upload`), the system **MUST** validate the declared mime_type
against the content of the **first uploaded part**, which contains the file's magic bytes / file signature. Validation
**MUST** occur when the first part is received — before subsequent parts are accepted. If the detected type does not
match the declared mime_type, the system **MUST** abort the multipart upload and reject all subsequent parts.

**Rationale**: Without content inspection, a client can declare `image/png` but upload an executable, trivially
bypassing file type policies. Content-type validation ensures declared types are trustworthy for downstream consumers
and policy enforcement. First-part validation for multipart uploads provides the same level of guarantee as single-part
validation — magic bytes reside at the start of the file and are always contained in the first part because backends
that support multipart upload (`cpt-cf-file-storage-fr-backend-capabilities`) enforce a minimum part size (e.g., 5 MB
for S3) that far exceeds the longest magic-byte sequence (~12 bytes). Backends without native multipart support reject
multipart uploads entirely, so no fallback is needed.
**Actors**: `cpt-cf-file-storage-actor-platform-user`, `cpt-cf-file-storage-actor-cf-gears`

### 5.2 Ownership & Access Control

#### File Ownership

- [ ] `p1` - **ID**: `cpt-cf-file-storage-fr-file-ownership`

The system **MUST** associate every file with `tenant_id` (mandatory, immutable) plus `owner_kind ∈ {user, app}` and
`owner_id`. `user` is a platform user; `app` is a Gear (e.g., LLM Gateway owning its generated media).
The owner principal is immutable after creation except through explicit ownership transfer
(`cpt-cf-file-storage-fr-ownership-transfer`) or owner deletion workflows (`cpt-cf-file-storage-fr-owner-deletion`).
`tenant_id` is never mutable.

**Rationale**: Ownership determines who can manage (delete, update metadata) a file and establishes the basis for
access control decisions. Separating `tenant_id` from `(owner_kind, owner_id)` reflects how Gears scopes data:
tenant is the hard boundary for isolation, while the owner identifies a specific principal within the tenant.
Gears own platform-generated content (LLM outputs, reports) via `owner_kind = app` without requiring an artificial
human user.
**Actors**: `cpt-cf-file-storage-actor-platform-user`, `cpt-cf-file-storage-actor-cf-gears`

#### Authorization Checks

- [ ] `p1` - **ID**: `cpt-cf-file-storage-fr-authorization`

The system **MUST** verify authorization for every file operation by requesting an access decision from the
Authorization Service. Read, write, and delete operations **MUST** be checked against `gts.cf.fstorage.file.type.v1~` resources in
the context of the requesting user. Authorization requests **MUST** include the file's GTS type
(`cpt-cf-file-storage-fr-file-type-classification`) in the resource context to enable per-type access decisions.

For content operations the read/write decision is made by the **control plane** when it issues the signed URL, and
the signed URL's constraints carry that authorization to the **sidecar**. When the sidecar must write metadata on the
user's behalf (e.g. binding an uploaded version), it calls the control plane under its **own app-token plus an
on-behalf-of `<user>`** claim, and the access decision is made against the **delegated user**, not the sidecar
identity.

**Rationale**: All file access must be governed by the platform's centralized authorization model to enforce role-based,
tenant-scoped, and type-scoped permissions. Delegation lets the sidecar act in the data path without becoming an
authorization principal in its own right.
**Actors**: `cpt-cf-file-storage-actor-platform-user`, `cpt-cf-file-storage-actor-cf-gears`

#### Tenant Boundary Enforcement

- [ ] `p1` - **ID**: `cpt-cf-file-storage-fr-tenant-boundary`

The system **MUST** enforce tenant isolation on every file operation: a principal in one tenant **MUST NOT**
access files owned by another tenant.

**Rationale**: Multi-tenant platforms require strict data isolation to prevent unauthorized cross-tenant access.
**Actors**: `cpt-cf-file-storage-actor-platform-user`, `cpt-cf-file-storage-actor-cf-gears`

#### Data Classification

- [ ] `p1` - **ID**: `cpt-cf-file-storage-fr-data-classification`

FileStorage treats all stored files as opaque binary blobs and does **NOT** inspect, classify, or label file content by
sensitivity level. Data classification (public, internal, confidential, restricted) is the responsibility of consuming
gears and policies. FileStorage enforces access control through its authorization model and tenant boundaries
regardless of data sensitivity.

**Rationale**: FileStorage is a general-purpose storage service that serves gears with diverse data sensitivity
requirements. Embedding classification logic in the storage layer would couple it to domain-specific semantics. Instead,
consuming gears classify their own data and rely on FileStorage's authorization and tenant isolation to enforce access
boundaries appropriate to the sensitivity level.
**Actors**: `cpt-cf-file-storage-actor-cf-gears`

#### File Type Classification

- [ ] `p1` - **ID**: `cpt-cf-file-storage-fr-file-type-classification`

The system **MUST** require a GTS file type identifier on every file at upload time. The file type classifies the file
by domain and purpose following the GTS type format (e.g. `gts.cf.fstorage.file.type.v1~x.genai.llm.autogenerated.v1~`
for LLM-generated files). The file type **MUST** be:

- Mandatory — uploads without a file type **MUST** be rejected
- Immutable — the file type **MUST NOT** be changeable after creation
- Stored as system-managed metadata — returned in all metadata queries alongside other system fields
- Validated — the system **MUST** verify that the provided type follows the GTS type format

The system **MUST** be able to use the file type to make per-type access decisions, enabling isolation
between actors and gears — a gear **MUST** only be able to access files of types it is authorized for. File type
authorization is enforced through the existing authorization model (`cpt-cf-file-storage-fr-authorization`).

**Rationale**: Without file type classification, any gear with general file access can read files created by any other
gear, breaking isolation between platform components. GTS types enable fine-grained, per-actor access control — e.g.,
the LLM Gateway can only access LLM-generated files, the Feedback gear can only access feedback-related files —
without requiring separate storage namespaces or custom authorization logic per gear.
**Actors**: `cpt-cf-file-storage-actor-platform-user`, `cpt-cf-file-storage-actor-cf-gears`

#### Ownership Transfer

- [ ] `p2` - **ID**: `cpt-cf-file-storage-fr-ownership-transfer`

The system **MUST** allow the current file owner to transfer ownership of a file to another principal (user or app)
within the **same tenant**. Cross-tenant transfer is **NOT** supported. Ownership transfer **MUST** be an audited
operation and **MUST** require authorization of both the current owner and the receiving principal.

**Rationale**: As teams and gears evolve, files may need to change hands. Restricting transfers to within the
file's tenant preserves the tenant-isolation invariant.
**Actors**: `cpt-cf-file-storage-actor-platform-user`

### 5.3 Sharing

FileStorage P1 exposes **only an authenticated REST surface**. Anonymous/public access, per-recipient grants,
expirations, content/version pinning, download counters, and any other sharing primitives are **out of P1 scope
and deferred to P3**.

The working name for the deferred capability is "FileShare". Whether it ships as a separate Gear or
as an extension of FileStorage itself is an open architectural decision to be settled by a future ADR at the
time the functionality is implemented. FileStorage P1 stores no sharing-related state, exposes no anonymous URL
namespace, and has no JWT-bypass paths — its surface is identical for every consumer and always goes through
platform authentication and the Authorization Service.

**Rationale**: Public/anonymous access is a sharing concern, not a storage concern. Keeping FileStorage purely
internal in P1 lets sharing semantics evolve independently inside a single gear with the appropriate data model, and
eliminates JWT-bypass surfaces and owner-private-header redaction logic from FileStorage.

### 5.4 Policies (Phase 2)

#### Allowed File Types Policy

- [ ] `p2` - **ID**: `cpt-cf-file-storage-fr-allowed-types-policy`

The system **MUST** allow owners to define policies specifying which file types (by mime_type) are permitted for
upload. Uploads of disallowed types **MUST** be rejected.

**Rationale**: Tenants need to restrict uploads to approved file types for security and compliance (e.g., blocking
executable files).
**Actors**: `cpt-cf-file-storage-actor-platform-user`

#### File Size Limits Policy

- [ ] `p2` - **ID**: `cpt-cf-file-storage-fr-size-limits-policy`

The system **MUST** enforce file size limits from two sources:

- **Backend limit** — each storage backend declares its maximum supported file size in configuration. This is a hard
  ceiling that no policy can override.
- **Policy limits** — tenants and users define a global maximum size and optional per-mime-type overrides (e.g., 100 MB
  general, 1 GB for `video/*`). When both tenant and user policies apply, the most restrictive value wins.

Uploads exceeding any applicable limit **MUST** be rejected with an error identifying which limit was violated.

**Rationale**: Backend limits reflect physical constraints of the storage system. Policy limits give tenants and users
granular control over storage consumption. The most-restrictive-wins model ensures no level can override another's
constraints.
**Actors**: `cpt-cf-file-storage-actor-platform-user`

#### File Events

- [ ] `p2` - **ID**: `cpt-cf-file-storage-fr-file-events`

The system **MUST** emit events to the EventBroker gear on file write operations (upload, update, delete). Owner
policy **MUST** define which event types are enabled.

**Rationale**: Enables integration with downstream consumers for workflows such as antivirus scanning, content
moderation, indexing, or backup triggers — without coupling FileStorage to specific consumers.
**Actors**: `cpt-cf-file-storage-actor-platform-user`

#### Storage Usage Reporting

- [ ] `p2` - **ID**: `cpt-cf-file-storage-fr-usage-reporting`

The system **MUST** report storage usage data to the Usage Collector service. Usage reports **MUST** include per-owner
storage consumption (total bytes, file count) and **MUST** be emitted on every write operation that changes storage
consumption (upload, delete, version creation, version deletion) and on ownership transfer
(`cpt-cf-file-storage-fr-ownership-transfer`). For ownership transfers, the system **MUST** emit a usage report for both
the previous owner (storage decrease) and the new owner (storage increase). The reporting mechanism **MUST** be
asynchronous and **MUST NOT** block file operations if the Usage Collector is temporarily unavailable.

**Rationale**: Centralized usage data is required for metering, billing, capacity planning, and analytics. Ownership
transfers shift per-owner storage consumption without changing total platform storage — without debit/credit reporting,
billing and quota data become stale after transfers. Asynchronous reporting ensures file operations are not degraded by
usage collection availability.
**Actors**: `cpt-cf-file-storage-actor-platform-user`, `cpt-cf-file-storage-actor-cf-gears`

#### Storage Quota Enforcement

- [ ] `p2` - **ID**: `cpt-cf-file-storage-fr-storage-quota`

The system **MUST** check with the Quota Enforcement service before accepting any operation that increases storage
consumption (including uploads and version creation). Operations that would exceed the owner's storage quota **MUST** be
rejected.

**Rationale**: Without storage quotas, tenants can consume unbounded storage, increasing costs and risking resource
exhaustion for the platform. Quota checks must cover all storage-consuming operations, not only initial uploads, to
prevent quota bypass through versioned overwrites.
**Actors**: `cpt-cf-file-storage-actor-platform-user`, `cpt-cf-file-storage-actor-cf-gears`

**Current status**: Storage quota is **not enforced in any deployment** — the Quota Enforcement service this
requirement depends on does not exist yet. `file-storage`'s consumer side (the `QuotaClient` port and its
fail-closed call sites) is implemented and ready to enforce the check once that service exists. Technical detail in
[DESIGN.md](./DESIGN.md) (`quota-adapter`) and [operations.md](./operations.md).

### 5.5 Metadata

#### Rich Metadata Storage

- [ ] `p1` - **ID**: `cpt-cf-file-storage-fr-metadata-storage`

The system **MUST** store and return the following system-managed metadata for every file:

- File name (original upload name)
- File size (bytes)
- File type (mime_type)
- GTS file type (`cpt-cf-file-storage-fr-file-type-classification`)
- Creation date
- Last modified date
- Owner (`owner_kind ∈ {user, app}` + `owner_id`) and `tenant_id`

In addition, the system **MUST** support user-defined custom metadata as arbitrary key-value string pairs. Custom
metadata **MUST** be specifiable at upload time and updatable after upload. The system **MUST** return custom metadata
alongside system-managed metadata in metadata queries.

**Rationale**: Rich metadata enables file browsing, search, validation, and governance across the platform. Custom
metadata enables consumers to attach domain-specific context (tags, categories, processing status, source identifiers)
without schema changes — following the established pattern used by S3 object metadata, GCS custom metadata, and Azure
Blob metadata.
**Actors**: `cpt-cf-file-storage-actor-platform-user`, `cpt-cf-file-storage-actor-cf-gears`

#### Update Custom Metadata

- [ ] `p1` - **ID**: `cpt-cf-file-storage-fr-update-metadata`

Any actor authorized for the **write** action on the file's GTS type
(`cpt-cf-file-storage-fr-authorization`) **MUST** be able to update the file's `custom_metadata` (user-defined
key-value pairs).

The set of principals admitted by the Authorization Service for this action **MAY** include the file's current owner,
other principals within the same tenant, or service identities — the model is policy-driven, not hard-coded to
"owner". All other system-managed metadata (`file_id`, `tenant_id`, `owner_kind`, `owner_id`, `name`, `size`,
`mime_type`, `gts_file_type`, `created_at`) is **NOT** user-updatable — it is maintained by the system. A successful
update **MUST** advance the file's last modified date.

**Rationale**: Custom metadata evolves as files are processed, categorized, or annotated by consuming gears. System
metadata reflects the immutable physical properties of the file and must remain authoritative. Routing the
authorization decision through `cpt-cf-file-storage-fr-authorization` (rather than hard-coding "only the owner can
update") keeps the access-control model centralized in the platform Authorization Service and lets tenants extend
write permission to additional principals (delegated maintainers, automation service identities, etc.) without
schema changes.
**Actors**: `cpt-cf-file-storage-actor-platform-user`, `cpt-cf-file-storage-actor-cf-gears`

#### Custom Metadata Limits

- [ ] `p2` - **ID**: `cpt-cf-file-storage-fr-metadata-limits`

The system **MUST** enforce configurable limits on custom metadata: maximum number of key-value pairs per file, maximum
key name length, maximum value length, and maximum total custom metadata size per file. Metadata operations exceeding
limits **MUST** be rejected.

**Rationale**: Without limits, custom metadata can be abused for general-purpose data storage, inflating metadata
storage costs and degrading query performance.
**Actors**: `cpt-cf-file-storage-actor-platform-user`, `cpt-cf-file-storage-actor-cf-gears`

### 5.6 File Retention & Lifecycle

#### Indefinite Retention

- [ ] `p1` - **ID**: `cpt-cf-file-storage-fr-retention-indefinite`

In phase 1, files **MUST** be retained indefinitely until explicitly deleted by an authorized actor
(`cpt-cf-file-storage-fr-authorization`). The system **MUST NOT** automatically delete or expire file content based on
age or inactivity.

**Rationale**: In the absence of tenant-level retention policies (phase 2), indefinite retention is the safest default —
it prevents accidental data loss and gives consuming gears predictable storage semantics.
**Actors**: `cpt-cf-file-storage-actor-platform-user`, `cpt-cf-file-storage-actor-cf-gears`

#### Retention Policies

- [ ] `p2` - **ID**: `cpt-cf-file-storage-fr-retention-policies`

The system **MUST** allow owners to define retention policies specifying automatic file expiration based on age,
inactivity, or custom metadata criteria. The system **MUST** also support per-file retention overrides set by the file
owner. When a file's retention period expires, the system **MUST** delete the file content, metadata, and all associated
links, and emit an audit record.

**Rationale**: Regulated environments and cost-conscious tenants need automated lifecycle management to enforce data
retention compliance and control storage growth.
**Actors**: `cpt-cf-file-storage-actor-platform-user`

#### Owner Deletion Handling

- [ ] `p2` - **ID**: `cpt-cf-file-storage-fr-owner-deletion`

The system **MUST** handle file owner removal (user or tenant deletion) by consuming owner deletion events from the
EventBroker. Upon receiving an owner deletion event, the system **MUST** execute a configurable workflow via the
Serverless Runtime to determine the disposition of all files owned by the deleted entity. The workflow **MUST** be able
to:

- Delete all files owned by the removed owner
- Archive files (mark as archived and disable further modifications while preserving content)
- Transfer ownership to another user or app within the same tenant
- Apply any combination of the above based on file metadata or custom criteria

The specific disposition logic **MUST** be defined as a Serverless Runtime workflow or function, configurable per
deployment. If no workflow is configured, the system **MUST** retain files indefinitely (no automatic deletion) and
mark them as orphaned for manual resolution.

**Rationale**: When users leave an organization or tenants are decommissioned, their files require deliberate handling —
blind deletion risks data loss, while indefinite retention risks compliance violations. Delegating disposition to
Serverless Runtime workflows enables deployment-specific logic (legal holds, data migration, cascading cleanup) without
embedding policy decisions in FileStorage.
**Actors**: `cpt-cf-file-storage-actor-platform-user`, `cpt-cf-file-storage-actor-cf-gears`

#### Orphan Reconciliation

- [ ] `p2` - **ID**: `cpt-cf-file-storage-fr-orphan-reconciliation`

The system **MUST** automatically detect and reconcile orphan state between the metadata store and storage backends.
Because content is uploaded to the sidecar and the version is only later **bound** in the metadata DB (the two writes
are not atomic), several edge cases produce orphans:

- A version was **pre-registered** (`status = pending`) and the bytes uploaded to the sidecar, but the **bind** never
  happened — the client abandoned the upload, or dropped after a failed-precondition response without retrying. The `pending` version row
  and its backend object are left dangling
- A backend object was written (`/{file_id}/{version_id}`) but no version row exists for it (the pre-register itself
  was lost)
- A **non-current** version superseded by a later bind and past the retention rule (`cpt-cf-file-storage-fr-retention-policies`)
- *(P2)* A multipart upload session was initiated but neither `complete` nor `abort` was invoked, leaving a `pending`
  version row and uploaded parts hanging

After a configurable grace period, the system **MUST** reconcile version rows against actual backend object existence
and apply the following dispositions:

- `pending` version past the grace window (never bound) → delete the version row **and** its backend object
- `available` version with **no** matching backend object → flag for operator attention (do **NOT** auto-delete; most
  likely backend data loss requiring manual review)
- Backend object with no matching version row → delete at the backend (orphaned content; no metadata path resolves it)
- Non-current version beyond the retention rule → delete the version row **and** its backend object
  (`cpt-cf-file-storage-fr-retention-policies`)
- *(P2)* Multipart session past the grace window with no `complete` → aborted at the backend
  (`abortMultipartUpload`), uploaded parts discarded, the corresponding `pending` version row removed

Reconciliation **MUST** be a control-plane internal scheduled task — it **MUST NOT** be triggerable from any public
API surface, it issues backend deletes via the sidecar — and **MUST** emit audit records
(`cpt-cf-file-storage-fr-audit-trail`) for every disposition it performs. This engine is unified with version
retention (`cpt-cf-file-storage-fr-retention-policies`): both prune by deleting a version row plus its backend object.

In **P1 there is no cleanup engine** — `pending`/non-current versions and orphan blobs accumulate (the indefinite-retention
P1 default, `cpt-cf-file-storage-fr-retention-indefinite`); acceptable at initial-release volumes.

**Rationale**: Upload-then-bind across two stores inevitably produces divergence on failure, and it accumulates as
`pending` rows pointing at blobs no consumer reached, or blobs with no row. Reconciliation keeps the two stores
converged. Auto-deletion is safe for orphan content (no metadata points to it) and for stale `pending` versions (the
bind never finished, so no consumer depends on them). The diverged-`available` case is the only one needing manual
handling, because it implies backend data loss that auto-deletion would mask.
**Actors**: `cpt-cf-file-storage-actor-cf-gears`

#### File Versioning

- [ ] `p1` - **ID**: `cpt-cf-file-storage-fr-file-versioning`

Versioning is a **FileStorage-level** feature and **MUST NOT** depend on a backend versioning capability: each version
is a **distinct immutable backend object** at `/{file_id}/{version_id}` and the file's live content is the
`content_id` pointer (`cpt-cf-file-storage-fr-upload-file`). It therefore works identically on every backend (S3, NAS,
FTP, …). The system **MUST**:

- Assign a FileStorage-owned `version_id` (a uuid) on every content write and create the corresponding immutable
  object; `version_id`s are treated as opaque uuids (never parsed for ordering — use creation time for order)
- Retrieve a specific version's content and metadata by `version_id`
- List all versions of a file (current and non-current) with each version's `version_id`, size, hash, creation
  timestamp, and whether it is the current version
- **Restore** a prior version by **re-binding** `content_id` to that version's `version_id` — a pointer swap, no
  re-upload (`cpt-cf-file-storage-fr-conditional-requests`). Restore **MUST** require the same authorization as a
  content write
- Permanently delete a specific version by `version_id` (removing its row and backend object); deleting the only
  remaining version is equivalent to deleting the file (`cpt-cf-file-storage-fr-delete-file`)

In P1 there is **no automatic version cleanup** — versions are retained indefinitely (the P1 default, see
`cpt-cf-file-storage-fr-retention-indefinite`). Automatic pruning by a version-retention policy (keep ≤ X versions
and/or younger than T) is a P2 concern, unified with orphan reconciliation in the P2 cleanup engine
(`cpt-cf-file-storage-fr-retention-policies`, `cpt-cf-file-storage-fr-orphan-reconciliation`).

The system **MUST** apply the same authorization, tenant boundary enforcement, and audit requirements to all versioned
operations as to current-version operations.

**Rationale**: Modelling each version as its own immutable object plus a pointer makes versioning a property of
FileStorage rather than of the backend, so it is uniform across heterogeneous backends, makes "replace content" and
"restore" cheap pointer swaps, and guarantees content is never mutated in place. Deferring automatic cleanup to P2
keeps P1 simple (indefinite retention is the safe default); accumulation is acceptable at initial-release volumes.
**Actors**: `cpt-cf-file-storage-actor-platform-user`, `cpt-cf-file-storage-actor-cf-gears`

#### Backend Migration

- [ ] `p2` - **ID**: `cpt-cf-file-storage-fr-backend-migration`

The system **MUST** be able to relocate a file's content from one storage backend to another **without changing the
file's `/files/{id}` URL or its identity**. Migration **MUST**:

- preserve the `file_id`, ownership, custom metadata, content hash, and externally observable behaviour of the file;
- be authorized as an administrative/owner operation and emit audit records (`cpt-cf-file-storage-fr-audit-trail`) per
  migrated file;
- update the file's `backend_id`/`backend_path` only after the destination object is durably written and verified
  (hash match), then remove the source object best-effort (a failed source cleanup degrades to an orphan handled by
  `cpt-cf-file-storage-fr-orphan-reconciliation`).

In P1 a file's backend is immutable; this requirement lifts that restriction for **non-versioned** files in P2.
Migration of versioned files (which carry a backend-owned version chain) is constrained by the backend's versioning
semantics and is out of scope until a dedicated design addresses version-chain relocation.

**Rationale**: One of the reasons to keep content behind the FileStorage sidecar (ADR-0003) is the ability to move
bytes between backends without rotating URLs. Real drivers include cost-tier optimization (move cold data to a cheaper
tier), backend deprecation/decommissioning, tenant data residency (relocate a tenant's files to an in-region backend),
capacity rebalancing across buckets, and disaster recovery from a degraded backend. Enforcing `backend_id`
immutability at the service layer only (not as a DB constraint) keeps this a behavioural change in P2 with no schema
migration.
**Actors**: `cpt-cf-file-storage-actor-cf-gears`

A content-hash-modes design (multipart offset-manifest composite hashing, computed on-the-fly) is recorded in
[ADR-0006](./ADR/0006-cpt-cf-file-storage-adr-content-hash-modes.md).

#### File Encryption

- [ ] `p3` - **ID**: `cpt-cf-file-storage-fr-file-encryption`

File encryption requires the server-side encryption backend capability (`cpt-cf-file-storage-fr-backend-capabilities`).
When the encryption capability is available for a backend, the system **MUST** support server-side encryption of file
content at rest, configurable per backend and per policy.

**Rationale**: Regulated environments and security-sensitive deployments require encryption at rest to meet compliance
requirements (GDPR, HIPAA, SOC 2) and protect stored data against unauthorized physical or logical access to the
storage backend.
**Actors**: `cpt-cf-file-storage-actor-platform-user`, `cpt-cf-file-storage-actor-cf-gears`

### 5.7 Audit

#### Audit Trail

- [ ] `p2` - **ID**: `cpt-cf-file-storage-fr-audit-trail`

The system **MUST** produce an audit record for every write operation (upload, content replacement, delete, metadata
update). Audit records **MUST** include the operation type, actor identity, file identifier, timestamp, and outcome
(success or failure).

**Rationale**: Audit trails are required for security forensics, compliance reporting, and operational troubleshooting.
**Actors**: `cpt-cf-file-storage-actor-platform-user`, `cpt-cf-file-storage-actor-cf-gears`

#### Read Audit Logging

- [ ] `p3` - **ID**: `cpt-cf-file-storage-fr-read-audit`

The system **MUST** support optional audit logging for read operations (downloads and metadata queries), configurable
per policy. When enabled by policy, the system **MUST** produce an audit record for every read operation. Because all
content traffic transits the sidecar, read audit applies uniformly to every download — there are no per-flow
carve-outs.

**Rationale**: Regulated environments and security-sensitive owners require visibility into who accessed their files and
when. Making read audit optional per policy avoids the performance and storage overhead of logging every read
across the platform, while enabling it where compliance demands it.
**Actors**: `cpt-cf-file-storage-actor-platform-user`, `cpt-cf-file-storage-actor-cf-gears`

### 5.8 Pluggable Storage Backends

#### Backend Abstraction

- [ ] `p1` - **ID**: `cpt-cf-file-storage-fr-backend-abstraction`

The system **MUST** abstract the storage layer behind a common interface, enabling support for multiple backend types (
S3, GCS, Azure Blob, NFS, FTP, SMB, WebDAV, local filesystem).

**Rationale**: Different deployments and tenants have different storage infrastructure; a common interface allows
backend selection without changing the gear's core logic.
**Actors**: `cpt-cf-file-storage-actor-cf-gears`

#### Backend Capabilities

- [ ] `p1` - **ID**: `cpt-cf-file-storage-fr-backend-capabilities`

The system **MUST** define a capability model for storage backends. Each backend **MUST** declare which optional
capabilities it supports. The system **MUST** support at least the following client-facing capabilities:

- **Multipart Upload** — the backend natively supports chunked upload with independent part transfers and server-side
  assembly
- **Server-Side Encryption** — the backend can encrypt file content at rest using backend-managed or customer-provided
  keys

Note: versioning is **not** a backend capability — it is implemented at the FileStorage level (distinct objects per
version + a pointer) and works on every backend (`cpt-cf-file-storage-fr-file-versioning`).

Backends **MAY** additionally support internal-only capabilities (e.g., presigned URL generation for
backend-to-backend replication, migration, or backup tooling). Internal-only capabilities are used by FileStorage
itself and are **NOT** exposed on the public capability discovery surface — no backend-addressable URL is ever
returned to a client.

Each declared client-facing capability **MUST** be independently configurable as enabled or disabled per backend. A
capability that is supported by the backend but disabled by configuration **MUST** behave identically to an
unsupported capability — the system **MUST NOT** expose or use it. Only capabilities that are both declared by the
backend and enabled in configuration are considered available.

The system **MUST** expose the set of available (declared and enabled) client-facing capabilities per backend so that
consumers can discover them at runtime. When a consumer requests an operation that depends on an unavailable
capability, the system **MUST** return a clear error indicating the capability is unavailable. Capability declarations
**MUST** be part of the backend configuration — not inferred at runtime from probing.

**Rationale**: Storage backends vary widely in feature support. A formal capability model enables FileStorage to adapt
behavior per backend, allows consumers to discover and handle feature availability, and replaces ad-hoc fallback logic
with a consistent, extensible pattern. Separating client-facing capabilities from internal-only ones preserves backend
opacity while keeping internal optimizations available to FileStorage itself.
**Actors**: `cpt-cf-file-storage-actor-cf-gears`

#### Backend Configuration Source

- [ ] `p1` - **ID**: `cpt-cf-file-storage-fr-backend-config-source`

In P1, storage backend configurations (`type`, `endpoint`, `credentials`, `capabilities`, `hash_policy`) **MUST** be
loaded from a static TOML configuration file at gear startup. Adding, removing, or re-configuring a backend
requires a gear restart. The configured set is exposed for read-only runtime introspection.

**Rationale**: A static configuration file is the simplest viable mechanism for P1 — no DB or admin-UI dependency.
Read-only HTTP introspection is sufficient for clients to discover available backends and their capabilities without
granting any runtime mutation surface.
**Actors**: `cpt-cf-file-storage-actor-cf-gears`

#### Runtime Backend Configuration

- [ ] `p3` - **ID**: `cpt-cf-file-storage-fr-runtime-backends`

The system **MUST** allow tenants to connect and configure storage backends at runtime without requiring service
rebuild or redeployment. Runtime backend configurations **MUST** be persisted in the metadata database (replacing the
P1 TOML source) and propagated to running gear instances.

**Rationale**: Enterprise tenants need to bring their own storage (BYOS) and switch backends based on cost, compliance,
or geographic requirements.
**Actors**: `cpt-cf-file-storage-actor-platform-user`

### 5.9 Access Interfaces

#### Control-Plane REST API

- [ ] `p1` - **ID**: `cpt-cf-file-storage-fr-rest-api`

The system **MUST** expose a control-plane REST API under a single auth-required namespace (`/api/file-storage/v1`)
for metadata management, listing, backend discovery, version bind, and the issuance of signed content URLs
(`cpt-cf-file-storage-fr-signed-urls`). This surface **MUST NOT** accept or return file content — content is moved
exclusively by the sidecar via signed URLs. FileStorage P1 has no anonymous namespace — see `§5.3`.

**Rationale**: REST is the standard control interface for Gears and platform UI; keeping content off this surface is
what allows the data plane (sidecar) to scale independently (ADR-0003).
**Actors**: `cpt-cf-file-storage-actor-platform-user`, `cpt-cf-file-storage-actor-cf-gears`

#### Signed Content URLs

- [ ] `p1` - **ID**: `cpt-cf-file-storage-fr-signed-urls`

The control plane **MUST** issue short-lived **signed URLs** that authorize a single content operation
(`GET`/`PUT`/part) against the **sidecar** for a specific object. Signed URLs **MUST**:

- be a **stateless, opaque PASETO `v4.public` token** (Ed25519), verifiable by the sidecar without a database lookup,
  for which the control plane holds the private key (**sole minter**) and the sidecar holds only the public key;
- be carried either in the `fs-token` URL query parameter (`?fs-token=<token>`, for bare embeddable URLs) or in the
  `X-FS-Token` request header (for programmatic/batch) — the **same token**, chosen by access intent; it is **never**
  carried in `Authorization`, which always carries the standard platform JWT;
- have a format known **only** to the control plane and the sidecar; every other participant (browser, CDN, proxy,
  app, logs) **MUST** treat the token as opaque bytes and **MUST NOT** parse it — the claim-set and crypto may change;
- always point at the sidecar, **never** at a backend-addressable URL (`cpt-cf-file-storage-principle-backend-opacity`);
- bind the **operation** `op` ∈ {GET, PUT, part} into the token (also checked against the HTTP method), so it cannot be
  reused for a different operation;
- carry **AND-combined claims**, of which only the expiry is mandatory:
  - **expiry** (`exp`, required) — and it **MUST NOT** exceed a configured maximum lifetime `max_url_ttl` (recommended
    7 days), enforced by the control plane at signing; the sidecar rejects once `now > exp`;
  - optional client `ip`/CIDR;
  - optional predicates over the caller's auth-token claims (e.g. `typ=user`, `sub=<id>`, `tenant_id=<id>`); when one
    is present the sidecar **MUST** also validate a real platform token and match each claim;
  - on **upload** URLs, optional content constraints the sidecar enforces during the stream: a size bound — either
    `max_size` (≤) **or** `exact_size` (==), mutually exclusive — and an `expected_hash` (`<alg>:<hex>`, `<alg>` from
    the backend allow-list) the uploaded bytes must match;
  - *(P2)* a `max_rate` (bandwidth) and a `max_conns` (concurrent connections) scoped to a single `(file_id, op)`;
- be tamper-evident as a whole — a client **MUST NOT** be able to add, remove, or weaken a constraint without
  invalidating the signature;
- optionally carry a set of response headers the sidecar **MUST** echo verbatim on the served response (e.g.
  `Content-Disposition`, `Content-Type` override, `Cache-Control`), so the sidecar needs no control-plane round-trip.

In P1 a single static signing keypair is used (a `kid` in the PASETO footer is reserved for P2 rotation; no per-token
revocation and no key rotation in P1; emergency access revocation is the platform auth module's token revocation). Key
rotation and a multi-key set are deferred to P2, as is enforcement
of the `max_rate` / `max_conns` constraints (which additionally require coordinating the multi-instance sidecar fleet
on a shared backend).

**Rationale**: Signed URLs let the control plane delegate the byte transfer to the sidecar without exposing backends
and without a per-request control round-trip on the data path. AND-combined constraints give per-link access control
(time-boxed, ip-boxed, principal-boxed) reusing the platform's own token claims.
**Actors**: `cpt-cf-file-storage-actor-platform-user`, `cpt-cf-file-storage-actor-cf-gears`

#### Random Read Access

- [ ] `p1` - **ID**: `cpt-cf-file-storage-fr-range-requests`

The **sidecar** download endpoint **MUST** support random (non-sequential) read access to arbitrary byte ranges of
stored content so that consumers can seek through large files efficiently — most importantly, so that media players
can scrub through videos and audio without re-downloading the file. Because the `Range` header is **not** part of the
signed-URL signature, a **single** signed download URL serves **many** range requests at different offsets until it
expires (random access without re-presigning).

**Rationale**: Without random read access, every seek in a video forces a full re-download from byte 0, which is
unusable for any clip longer than a few seconds. The protocol-level mechanics (HTTP `Range`/`Content-Range` semantics,
`Accept-Ranges` advertisement, backend-level range translation) are documented in DESIGN.md.
**Actors**: `cpt-cf-file-storage-actor-platform-user`, `cpt-cf-file-storage-actor-cf-gears`

### 5.10 Cache & Idempotency

#### Conditional Requests

- [ ] `p1` - **ID**: `cpt-cf-file-storage-fr-conditional-requests`

The system **MUST** support conditional HTTP requests (RFC 7232) across both planes — on the sidecar for downloads,
and on the control plane for metadata reads, version bind, content-replacement, metadata updates, and deletes. The
system **MUST**:

- Return an `ETag` header on every download (sidecar) and metadata (control) response. ETag is opaque, derived from
  `(file_id, content_id)`, and **MUST NOT** equal the content hash (which is exposed separately). Because `content_id`
  is the current version pointer, the ETag changes exactly when content is (re)bound
- Support `If-None-Match` on download/metadata reads — return `304 Not Modified` when the ETag matches
- Support `If-Match` on reads — return `400 failed_precondition` when the ETag does not match
- Require `If-Match` on every content **bind** (the optimistic CAS that swaps `content_id`) and on `DELETE` —
  `400 failed_precondition` on mismatch. The retry re-binds the already-uploaded `version_id` without re-upload.
  The bind may also execute **inside** the upload itself (`bind: "auto"`, the
  default on `POST /files`) — the CAS requirement is unchanged, only the transport differs: multipart `complete`
  reuses its own `If-Match` (absent → the first-content `content_id IS NULL` case) as the embedded bind's
  precondition, and the single-part finalize binds strictly under `content_id IS NULL` (first content of a new file
  only). A lost CAS never fails the upload: it is reported (`bind_state: "conflict"` / `X-FS-Bound: conflict` with
  the current ETag) and resolved by the same manual re-bind, still with no re-upload

**ETag is content-only.** Metadata-only updates bump `meta_version` and `last_modified_at` but **MUST NOT** change the
ETag or content hash — both remain tied to the content. Consequently `If-Match` on a metadata-only update protects
against concurrent **content** writes but does **not** detect concurrent metadata writes. To give callers lost-update
protection for metadata without coupling it to the content ETag, the system **MUST** support an optional
metadata-revision precondition on metadata-only updates (matched against `meta_version`, returning
`400 failed_precondition` on mismatch);
when the caller omits it, metadata updates remain last-write-wins (S3-style) for back-compatibility. See DESIGN
`cpt-cf-file-storage-principle-content-only-etag`.

**Rationale**: Conditional downloads eliminate redundant bandwidth for unchanged files and enable downstream caching by
browsers and reverse proxies. Conditional updates prevent silent data loss when multiple clients modify file metadata
concurrently. Both follow standard HTTP semantics (RFC 7232) understood by all HTTP clients. Since FileStorage manages
file metadata for all backends, ETags are a FileStorage-level feature independent of backend capabilities.
**Actors**: `cpt-cf-file-storage-actor-platform-user`, `cpt-cf-file-storage-actor-cf-gears`

#### Upload Idempotency

- [ ] `p2` - **ID**: `cpt-cf-file-storage-fr-upload-idempotency`

The system **MUST** support idempotent uploads. A client **MUST** be able to provide a unique idempotency key with an
upload request. If a subsequent upload request arrives with the same idempotency key, the system **MUST** return the
result of the original upload instead of creating a duplicate file. Idempotency keys **MUST** expire after a
configurable window.

Idempotency keys **MUST** be scoped to the file owner specified in the upload request — the same entity that will own
the resulting file (`cpt-cf-file-storage-fr-file-ownership`). When the owner is a tenant, the key is unique within that
tenant's namespace. When the owner is a user, the key is unique within that user's namespace. The same key value used by
different owners **MUST** be treated as distinct keys. The system **MUST NOT** allow idempotency key lookups to cross
owner boundaries — a request **MUST NOT** be able to detect whether a different owner has used a given key.

**Rationale**: Upload requests can fail ambiguously — the connection drops but the upload succeeds server-side. Without
idempotency, client retries create duplicate files. Idempotency keys enable safe retries for single-part and multipart
uploads across unreliable networks. Owner-scoped key namespacing prevents cross-tenant information leaks and aligns with
the platform's tenant boundary enforcement (`cpt-cf-file-storage-fr-tenant-boundary`).
**Actors**: `cpt-cf-file-storage-actor-platform-user`, `cpt-cf-file-storage-actor-cf-gears`

## 6. Non-Functional Requirements

### 6.1 Gear-Specific NFRs

#### Metadata Query Latency

- [ ] `p1` - **ID**: `cpt-cf-file-storage-nfr-metadata-latency`

File metadata queries **MUST** complete within 25ms at p95.

**Threshold**: <25ms p95
**Rationale**: Metadata queries are used for pre-fetch validation in latency-sensitive paths (e.g., a gear checks file
size before processing).
**Architecture Allocation**: See DESIGN.md § NFR Allocation for how this is realized

#### Content Transfer Latency

- [ ] `p1` - **ID**: `cpt-cf-file-storage-nfr-transfer-latency`

Content download latency **MUST** have no fixed overhead exceeding 50ms at p95; total transfer time is proportional to
file size.

**Threshold**: <50ms + transfer time p95
**Rationale**: The sidecar serves content synchronously in the request paths of consuming gears; excessive fixed
overhead compounds across requests with multiple files. (Allocated to the sidecar, per
ADR-0003.)
**Architecture Allocation**: See DESIGN.md § NFR Allocation for how this is realized

#### URL Availability

- [ ] `p1` - **ID**: `cpt-cf-file-storage-nfr-url-availability`

Stored file URLs **MUST** remain accessible for the duration of the file's retention with availability matching
the platform SLA.

**Threshold**: URL availability matches platform SLA for the duration of the retention period
**Rationale**: Consumers depend on URL stability — broken URLs disrupt downstream workflows and user experience.
**Architecture Allocation**: See DESIGN.md § NFR Allocation for how this is realized

#### Audit Completeness

- [ ] `p2` - **ID**: `cpt-cf-file-storage-nfr-audit-completeness`

Audit records **MUST** be emitted for 100% of write operations with no silent drops under normal operating conditions.

**Threshold**: 100% audit coverage for write operations
**Rationale**: Incomplete audit trails undermine compliance and forensic investigations.
**Architecture Allocation**: See DESIGN.md § NFR Allocation for how this is realized

#### Data Durability and Recovery

- [ ] `p1` - **ID**: `cpt-cf-file-storage-nfr-durability`

File content and metadata **MUST** achieve a Recovery Point Objective (RPO) of zero for committed writes — no
acknowledged upload may be silently lost. The Recovery Time Objective (RTO) for service restoration after an outage
**MUST NOT** exceed 15 minutes. These targets apply to the FileStorage service layer; underlying storage backend
durability (e.g., S3 99.999999999% durability) is inherited from the backend and not controlled by FileStorage.

**Threshold**: RPO = 0 (no data loss for committed writes); RTO ≤ 15 minutes
**Rationale**: File loss after a successful upload acknowledgment breaks consumer trust and disrupts downstream
workflows. The RPO=0 target ensures write-ahead semantics where acknowledgment implies durability. The 15-minute RTO
balances recovery speed with operational complexity for a non-user-facing backend service.
**Architecture Allocation**: See DESIGN.md § NFR Allocation for how this is realized

#### Scalability & Capacity

- [ ] `p1` - **ID**: `cpt-cf-file-storage-nfr-scalability`

FileStorage **MUST** support horizontal scaling to handle concurrent file operations without degradation. The system
**MUST** support at least 1,000 concurrent file operations (uploads + downloads + metadata queries combined) per
deployment instance. The system **MUST** scale linearly — adding instances **MUST** proportionally increase throughput
without introducing coordination bottlenecks between instances.

**Threshold**: ≥1,000 concurrent operations per instance; linear horizontal scaling
**Rationale**: As platform adoption grows, file operation volume grows proportionally. Without explicit scalability
requirements, the architecture may adopt patterns (global locks, shared mutable state) that prevent horizontal scaling.
**Architecture Allocation**: See DESIGN.md § NFR Allocation for how this is realized

#### Bandwidth & Egress

- [ ] `p1` - **ID**: `cpt-cf-file-storage-nfr-bandwidth`

Because every uploaded and downloaded byte transits the **sidecar** (per
ADR-0003 — backends are never addressed directly by clients),
**bandwidth, not CPU or memory, is the binding capacity constraint of the sidecar** (the control plane carries no
content and is not bandwidth-bound). Each sidecar instance **MUST** sustain a defined combined ingress+egress budget,
and aggregate transfer capacity **MUST** scale horizontally by adding stateless sidecar instances, independently of
the control plane. Repeat-read egress **MUST** be offloadable to an upstream caching layer (API-Gateway / CDN) using
the conditional-request headers the sidecar emits (`ETag`, `Cache-Control`, `Vary`), so that cache hits do not
re-transit the sidecar.

**Threshold**: ≥ 2.5 GiB/s combined ingress+egress per sidecar instance (≈ 25 GbE class); aggregate capacity =
`ceil(peak aggregate transfer rate / per-instance budget)` sidecar instances; conditional re-reads served from
CDN/proxy cache without sidecar egress
**Rationale**: ADR-0003 confines the terabyte-scale traffic to the sidecar. If the
NFR set only constrains CPU/memory (the scalability NFR), implementers may size and scale against the wrong dimension
and under-provision network capacity. Making the bandwidth budget explicit, allocating it to the sidecar, and making
download caching a first-class offload path keeps the data plane affordable at scale.
**Architecture Allocation**: See DESIGN.md § NFR Allocation for how this is realized

### 6.2 NFR Exclusions

None — all project-default NFRs apply to this gear.

### 6.3 Applicability Notes

The following NFR categories from the platform checklist are **not applicable** to this gear:

| Category                 | Rationale                                                                                                                                                                                                                                                                                               |
|--------------------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **Safety**               | FileStorage is a data storage service with no physical actuators, safety-critical control loops, or human safety implications.                                                                                                                                                                          |
| **UX**                   | FileStorage is a backend service consumed via SDK and APIs. It has no user-facing UI; UX concerns are the responsibility of consuming gears and platform UI.                                                                                                                                          |
| **Internationalization** | FileStorage stores and returns opaque binary content and metadata strings. It does not render, translate, or localize content. File names and metadata values are preserved as-is.                                                                                                                      |
| **Privacy by Design**    | FileStorage treats all files as opaque blobs and does not inspect, index, or process file content. Privacy controls (data minimization, consent, right to erasure) are enforced at the platform and consuming-gear level. Tenant isolation and access control are covered by functional requirements. |
| **Compliance**           | FileStorage does not implement domain-specific compliance logic (GDPR, HIPAA, SOX). It provides the building blocks (audit trail, tenant isolation, retention policies, encryption) that enable consuming gears and platform operators to achieve compliance.                                         |
| **Operations**           | Operational concerns (deployment, monitoring, alerting, runbooks) follow platform-wide standards and are not gear-specific.                                                                                                                                                                           |
| **Maintainability**      | Maintainability follows platform-wide coding standards, testing requirements, and CI/CD practices. No gear-specific maintainability NFRs beyond the platform baseline.                                                                                                                                |

## 7. Public Library Interfaces

### 7.1 Public API Surface

#### FileStorage SDK Trait

- [ ] `p1` - **ID**: `cpt-cf-file-storage-interface-sdk-trait`

**Type**: Rust trait (SDK crate)
**Stability**: unstable
**Description**: Async trait providing upload, download (seekable / with Range), delete, metadata read/update,
listing, version listing/restore, and backend-capability discovery. The SDK performs the two-step (presign +
sidecar transfer) **inside the consumer's process** — the control-plane service never streams bytes — so a consuming
gear sees a normal seekable read/write (`cpt-cf-file-storage-component-sdk-facade`).
**Breaking Change Policy**: Major version bump required for trait signature changes.

#### Control-Plane REST API

- [ ] `p1` - **ID**: `cpt-cf-file-storage-interface-rest-api`

**Type**: REST API (OpenAPI 3.0)
**URL Prefix**: `/api/file-storage/v1`
**Stability**: unstable
**Description**: HTTP REST API for authenticated metadata operations, listing, backend discovery, version bind, and
signed-URL issuance. It does **not** carry file content — content moves over signed URLs against the sidecar
(`cpt-cf-file-storage-fr-signed-urls`). All endpoints require platform JWT — there is no anonymous surface in P1 (see
`§5.3`).
**Breaking Change Policy**: Major version bump required for endpoint removal or incompatible schema changes.

#### Sidecar Data-Plane API

- [ ] `p1` - **ID**: `cpt-cf-file-storage-interface-sidecar-api`

**Type**: HTTP (signed-URL authorized)
**Stability**: unstable
**Description**: The sidecar's content surface (`GET`/`PUT`/part), addressed only via control-plane-issued signed
URLs and served from its own domain. Verifies the PASETO `v4.public` token and its claims, validates the platform token
when a token-claim predicate is present, serves `Range` and conditional requests, and echoes the response headers
baked into the URL. It holds **no** backend/tenant/user policy or quota state — all such limits (storage quota,
allowed types, size policy, retention) live in the control plane and are applied at presign; the sidecar enforces only
the per-URL constraints the signature carries, plus the per-URL connection/rate caps (P2).
**Breaking Change Policy**: Signed-URL format changes are coordinated with the control plane (shared signing contract).

### 7.2 External Integration Contracts

#### Gear Contract

- [ ] `p1` - **ID**: `cpt-cf-file-storage-contract-cf-gears`

**Direction**: provided by library (consumed by Gears)
**Protocol/Format**: In-process Rust SDK trait via ClientHub
**Compatibility**: Trait versioned with SDK crate; breaking changes require coordinated release with consuming gears.

#### Authorization Service Contract

- [ ] `p1` - **ID**: `cpt-cf-file-storage-contract-authz`

**Direction**: required from external service (Authorization Service)
**Protocol/Format**: Access decision requests for `gts.cf.fstorage.file.type.v1~` resources
**Compatibility**: Contract follows platform authorization protocol; changes require coordinated release.

#### Usage Collector Contract

- [ ] `p2` - **ID**: `cpt-cf-file-storage-contract-usage-collector`

**Direction**: required from external service (Usage Collector)
**Protocol/Format**: Asynchronous per-owner usage reports (storage consumption per owner, including ownership-transfer
debits/credits per `cpt-cf-file-storage-fr-usage-reporting`)
**Compatibility**: Contract follows platform usage reporting protocol; changes require coordinated release.

#### Quota Enforcement Contract

- [ ] `p2` - **ID**: `cpt-cf-file-storage-contract-quota-enforcement`

**Direction**: required from external service (Quota Enforcement)
**Protocol/Format**: Synchronous per-owner quota check requests before storage-consuming operations
(per `cpt-cf-file-storage-fr-storage-quota`)
**Compatibility**: Contract follows platform quota enforcement protocol; changes require coordinated release.

**Current status**: The Quota Enforcement counterparty does not exist, so this contract is not exercised in any
deployment. `file-storage`'s side is implemented and ready. See [DESIGN.md](./DESIGN.md).

#### EventBroker Contract

- [ ] `p2` - **ID**: `cpt-cf-file-storage-contract-eventbroker`

**Direction**: bidirectional (publishes file events; consumes platform events such as owner deletion)
**Protocol/Format**: Asynchronous event publishing and consumption via EventBroker gear
**Compatibility**: Contract follows platform event protocol; event schema changes require coordinated release.

#### Serverless Runtime Contract

- [ ] `p2` - **ID**: `cpt-cf-file-storage-contract-serverless-runtime`

**Direction**: required from external service (Serverless Runtime)
**Protocol/Format**: Workflow invocation for configurable lifecycle operations (e.g., owner deletion disposition)
**Compatibility**: Contract follows platform Serverless Runtime invocation protocol; changes require coordinated release.

## 8. Use Cases

### Upload a File

- [ ] `p1` - **ID**: `cpt-cf-file-storage-usecase-upload`

**Actor**: `cpt-cf-file-storage-actor-platform-user`

**Preconditions**:

- User is authenticated
- Authorization Service grants write access

**Main Flow**:

1. User asks the control plane to upload, supplying metadata (name, mime_type, GTS file type)
2. Control plane validates the GTS file type format and checks authorization for write on
   `gts.cf.fstorage.file.type.v1~` with the file type in resource context
3. *(Phase 2)* Control plane validates against policies (type, size); in phase 1 all uploads are accepted
4. Control plane returns a **signed upload URL** to the sidecar (`cpt-cf-file-storage-fr-signed-urls`)
5. User transfers the bytes to the **sidecar** at that URL; the sidecar streams to the backend object
   `/{file_id}/{version_id}`, computes the hash, and (on behalf of the user) **binds** the new version as current
   under optimistic CAS
6. *(Phase 2)* Audit record emitted for the upload
7. The client holds the `file_id` and the bound `version_id`; on a bind conflict (`400 failed_precondition`) it re-binds without
   re-uploading

**Postconditions**:

- File stored with metadata and ownership
- File is readable only by principals authorized via `cpt-cf-file-storage-fr-authorization`
- *(Phase 2)* Audit record emitted for the upload

**Alternative Flows**:

- **Missing or invalid GTS file type**: FileStorage rejects the upload with a validation error
- **Authorization denied**: FileStorage returns access-denied error
- *(Phase 2)* **Policy violation**: FileStorage returns error indicating which policy was violated (type or size)

### Fetch File for Gear Processing

- [ ] `p1` - **ID**: `cpt-cf-file-storage-usecase-fetch-media`

**Actor**: `cpt-cf-file-storage-actor-cf-gears`

**Preconditions**:

- File exists at the specified URL

**Main Flow**:

1. Gear asks the control plane for a download URL for the file
2. Control plane checks authorization for read on `gts.cf.fstorage.file.type.v1~` with the file's GTS type in resource
   context and returns a **signed download URL** to the sidecar (pinning the current `content_id`), plus metadata
3. Gear fetches the bytes from the **sidecar** at that URL (with `Range` for partial/seeking reads)
4. Sidecar streams the content from the backend; metadata (mime_type, size, GTS file type) came from step 2

**Postconditions**:

- Content and metadata returned to the requesting gear

**Alternative Flows**:

- **File not found**: FileStorage returns file_not_found error
- **Authorization denied**: FileStorage returns access-denied error

### Validate File Metadata Before Processing

- [ ] `p1` - **ID**: `cpt-cf-file-storage-usecase-get-metadata`

**Actor**: `cpt-cf-file-storage-actor-cf-gears`

**Preconditions**:

- File exists at the specified URL

**Main Flow**:

1. Gear calls get_metadata with a file URL
2. FileStorage checks authorization for read on `gts.cf.fstorage.file.type.v1~` with the file's GTS type in resource context
3. FileStorage returns metadata (name, size, mime_type, GTS file type, owner, availability) without transferring content

**Postconditions**:

- Metadata returned; no content transferred

**Alternative Flows**:

- **File not found**: FileStorage returns file_not_found error
- **Authorization denied**: FileStorage returns access-denied error

### Delete a File

- [ ] `p1` - **ID**: `cpt-cf-file-storage-usecase-delete-file`

**Actor**: `cpt-cf-file-storage-actor-platform-user`

**Preconditions**:

- User is authenticated
- User owns the file

**Main Flow** (delete the whole file):

1. Owner requests deletion of a file by its identifier
2. Control plane checks authorization for delete on `gts.cf.fstorage.file.type.v1~`
3. Control plane removes the file metadata, ownership records, and **all** version rows (metadata-row-first)
4. Control plane deletes the backend objects best-effort via the sidecar (a failed backend delete degrades to an
   orphan reconciled by the P2 cleanup engine)
5. *(Phase 2)* Control plane emits audit record for the deletion

**Postconditions**:

- Metadata, ownership, and all versions removed; subsequent requests for the file return `404` (idempotent re-delete)
- Backend objects removed (best-effort; residual orphans swept by `cpt-cf-file-storage-fr-orphan-reconciliation`)
- *(Phase 2)* Audit record emitted

**Alternative Flow — delete a single version** (`cpt-cf-file-storage-fr-file-versioning`):

1. Owner requests deletion of a specific version by `file_id` and `version_id`
2. Control plane checks authorization for delete on `gts.cf.fstorage.file.type.v1~`
3. Control plane removes that version's row and backend object
4. *(Phase 2)* Control plane emits audit record for the version deletion

**Postconditions**:

- The specified version is permanently removed; remaining versions unaffected
- Deleting the only remaining version is equivalent to deleting the file (Main Flow postconditions)
- Deleting the **current** version requires the file to have another version to fall back to, or the file is deleted
- *(Phase 2)* Audit record emitted

**Alternative Flows — error cases**:

- **Authorization denied**: FileStorage returns access-denied error
- **File not found**: FileStorage returns file_not_found error
- **Version not found**: FileStorage returns version_not_found error
- **Cross-tenant attempt**: FileStorage returns access-denied error (tenant boundary enforcement)

### Multi-Backend Deployment

- [ ] `p1` - **ID**: `cpt-cf-file-storage-usecase-backend-config`

**Actor**: `cpt-cf-file-storage-actor-cf-gears`

**Preconditions**:

- FileStorage is deployed with a configured storage backend

**Main Flow**:

1. Deployment A configures FileStorage with an S3-compatible backend (e.g., AWS S3)
2. Deployment B configures FileStorage with a different backend (e.g., Azure Blob Storage)
3. Both deployments expose identical FileStorage SDK and REST APIs
4. Gears interact with FileStorage through the SDK trait without awareness of the underlying backend
5. Upload, download, delete, metadata, and link operations behave identically regardless of backend

**Postconditions**:

- All functional requirements are met identically across different backend configurations
- Consuming gears require zero code changes when the backend changes

**Alternative Flows**:

- **Backend-specific feature unavailable**: FileStorage returns a clear error indicating the capability is unavailable
  (e.g., multipart upload or versioning request rejected when backend does not declare the capability)

### Configure Policy

- [ ] `p2` - **ID**: `cpt-cf-file-storage-usecase-configure-policy`

**Actor**: `cpt-cf-file-storage-actor-platform-user`

**Preconditions**:

- User has tenant administration privileges (for tenant-level policy) or is an authenticated user (for user-level
  policy)

**Main Flow**:

1. Tenant admin or user defines policies: allowed file types, size limits (global and per-type), enabled event types,
   and permitted sharing models
2. FileStorage validates and stores the policy configuration
3. Subsequent file operations are enforced against the effective policy (most restrictive per aspect across tenant and
   user levels)

**Postconditions**:

- Policy active and enforced on all file operations

**Alternative Flows**:

- **Invalid policy**: FileStorage returns validation error with details

## 9. Acceptance Criteria

- [ ] File upload returns persistent URL and stores metadata (name, size, type, dates, owner)
- [ ] File download returns content with correct metadata
- [ ] File deletion of a non-versioned file permanently removes content; the metadata row is removed before the
  best-effort backend delete, so a deleted file never leaves a row pointing at missing content, and re-deleting an
  already-deleted file is idempotent (`404`)
- [ ] Deleting a file removes all of its versions (metadata-row-first, idempotent); a single version can be deleted by `version_id`
- [ ] Authorization checked for every file operation via Authorization Service
- [ ] Tenant boundary enforced — cross-tenant access rejected
- [ ] Audit record emitted for every write operation
- [ ] Policies enforce file type and size restrictions on upload (most restrictive wins across tenant and user levels)
- [ ] All content traffic flows through the **sidecar** via signed URLs; no backend-addressable URL is returned to any client
- [ ] Content upload and download are each a two-step exchange (control request → signed URL → byte transfer to/from the sidecar); the control REST surface never carries content
- [ ] The credential is an opaque PASETO `v4.public` token (Ed25519), carried in the query (`?fs-token=`) or a header, stateless, enforcing AND-combined claims (expiry, optional ip, optional token-claim predicates, upload size/hash); altering any claim invalidates the signature; only control+sidecar parse it
- [ ] file_not_found error returned for non-existent files
- [ ] access_denied error returned for unauthorized operations
- [ ] Metadata-only queries complete without transferring file content
- [ ] Content is mutable through dedicated content-replacement operations; ETag (content-derived) changes on every
  content write; metadata-only updates do not change ETag or content hash
- [ ] Content replacement uploads a new immutable version and **binds** it as current under `If-Match` CAS; a
  conflicting bind returns `400 failed_precondition` and is retried by re-binding the already-uploaded `version_id` without re-uploading
  the bytes; backend content is never mutated in place
- [ ] `custom_metadata` is updatable by any actor authorized for the **write** action on the file's GTS type;
  system-managed metadata is not user-updatable
- [ ] Custom metadata update changes the file's last modified date
- [ ] File ownership (`owner_kind`, `owner_id`) is immutable after creation except through explicit ownership transfer
  or owner deletion workflows; `tenant_id` is never mutable
- [ ] Every file has a mandatory GTS file type assigned at upload time; uploads without a file type are rejected
- [ ] GTS file type is immutable after creation
- [ ] Authorization requests include the file's GTS type, enabling per-type access decisions
- [ ] A gear authorized only for type A cannot access files of type B
- [ ] FileStorage SDK and REST API behave identically regardless of configured storage backend
- [ ] File listing returns metadata only, is paginated, and requires a mandatory owner-kind filter (`user` or `app`)
- [ ] Multipart upload assembles parts into a complete file with correct metadata
- [ ] Upload rejected when declared mime_type does not match actual file content
- [ ] Each backend declares its supported client-facing capabilities (multipart upload, server-side encryption);
  internal-only capabilities are not surfaced on public discovery
- [ ] Consumers can discover backend capabilities at runtime
- [ ] Operations requiring an unsupported capability return a clear error
- [ ] Versioning is FileStorage-level and backend-agnostic: each content write creates a new immutable version at
  `/{file_id}/{version_id}`; metadata-only updates do not create a new version
- [ ] All versions of a file are listable with `version_id`, size, hash, timestamp, and current-version flag
- [ ] Restore rebinds `content_id` to a prior version (pointer swap, no re-upload), under the same authorization as a
  content write
- [ ] In P1 versions are retained indefinitely (no automatic cleanup); P2 prunes via the retention policy +
  reconciliation engine
- [ ] Permanent delete of a specific version removes only that version
- [ ] Declared capabilities are independently configurable (enable/disable) per backend
- [ ] A capability disabled by configuration behaves identically to an unsupported capability
- [ ] Download and metadata responses include `ETag` header derived from `(file_id, content_id)` and not equal
  to the content hash
- [ ] Conditional download with `If-None-Match` returns `304 Not Modified` when file is unchanged
- [ ] `If-Match` is required on content **bind** and on `DELETE`; missing or mismatching `If-Match` returns `400 failed_precondition`
- [ ] An optional metadata-revision precondition on metadata-only updates returns `400 failed_precondition` on mismatch, giving
  lost-update protection for concurrent metadata writers; when omitted, metadata updates remain last-write-wins
- [ ] An upload whose bind never completes leaves no current pointer to it; the orphan `pending` version and its blob
  are reconciled by the P2 cleanup engine (`cpt-cf-file-storage-fr-orphan-reconciliation`)
- [ ] Retried upload with the same idempotency key returns the original result without creating a duplicate file
- [ ] Retried upload with the same idempotency key by a different owner does not return or create the original owner's
  file
- [ ] Owner deletion event from EventBroker triggers a configurable Serverless Runtime workflow for file disposition
- [ ] Files of a deleted owner are retained as orphaned when no workflow is configured
- [ ] Server-side encryption is applied when the encryption capability is available and enabled for the backend
- [ ] Upload rejected when storage quota would be exceeded (Quota Enforcement service check)
- [ ] Usage report emitted asynchronously on every storage-consuming write operation; file operations not blocked if
  Usage Collector is unavailable
- [ ] Ownership transfer emits usage reports for both previous and new owner
- [ ] File events emitted to EventBroker on write operations (upload, update, delete) when enabled by owner policy
- [ ] HTTP Range requests return partial content for downloads; seeking and resumable downloads supported;
  `Accept-Ranges: bytes` set on every download response
- [ ] Retention policies automatically expire and delete files based on configured age, inactivity, or custom metadata
  criteria; per-file retention overrides are honored
- [ ] Storage backends in P1 are loaded from a static TOML configuration file at gear startup; in P3, backends can
  be connected and configured at runtime via admin API without service rebuild
- [ ] File ownership transferable by current owner to another user or app within the same tenant; transfer requires
  authorization of both parties and emits an audit record
- [ ] Custom metadata operations rejected when exceeding configurable limits (max pairs, key length, value length, total
  size)
- [ ] Read audit records emitted for every download when enabled by policy

## 10. Dependencies

| Dependency            | Description                                                        | Criticality |
|-----------------------|--------------------------------------------------------------------|-------------|
| ToolKit Framework      | Gear lifecycle, ClientHub for service registration               | p1          |
| Authorization Service | Access decisions for `gts.cf.fstorage.file.type.v1~` resources     | p1          |
| Audit Infrastructure  | Platform audit event sink                                          | p2          |
| Usage Collector       | Receives storage usage reports for metering and billing            | p2          |
| Quota Enforcement     | Per-owner storage quota enforcement                                | p2          |
| EventBroker           | Publishes and consumes file/platform events                        | p2          |
| Serverless Runtime    | Executes configurable workflows for lifecycle operations           | p2          |

## 11. Assumptions

- Authorization Service is available and supports `gts.cf.fstorage.file.type.v1~` resource type
- All file access respects tenant boundaries at the platform level
- Initial storage backend is configured at deployment time; runtime backend switching is phase 2
- The control-plane API requires platform JWT in P1; content is reached only via short-lived signed URLs against the
  sidecar, which carry their own AND-combined constraints. Any external/anonymous sharing is deferred to P3 (see `§5.3`)
- The control plane and the sidecar share the metadata DB (the sidecar reaches it via the FS SDK) and a signing
  keypair (private on control, public on the sidecar)
- Policy configuration is available to tenant administrators and users through the platform

## 12. Risks

| Risk                                                                | Impact                                                         | Mitigation                                                                                                                                              |
|---------------------------------------------------------------------|----------------------------------------------------------------|---------------------------------------------------------------------------------------------------------------------------------------------------------|
| Storage service unavailability blocks all file-dependent operations | High — multimodal AI, document workflows disrupted             | Design for graceful degradation; clear error propagation to consumers                                                                                   |
| Large file sizes increase request latency for consuming gears     | Medium — slow responses for multimodal and document operations | Metadata pre-fetch enables size validation; streaming support for large files                                                                           |
| Backend credential compromise enables unauthorized backend access  | High — data exposure                                           | Backend credentials held only by FileStorage and never exposed to clients (proxy model — see DESIGN.md); standard credential rotation procedures apply at the FileStorage layer |
| Policy misconfiguration blocks legitimate uploads                   | Medium — user frustration                                      | Policy validation on save; clear error messages identifying which policy was violated                                                                   |

## 13. Open Questions

None.

## 14. Traceability

- **Design**: [DESIGN.md](./DESIGN.md)
- **ADRs**: [ADR/](./ADR/)
- **Features**: [features/](./features/)
