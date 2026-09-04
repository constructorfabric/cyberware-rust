# FileStorage — Concurrency & Failure Model of the Upload Flow

**ID**: `cpt-cf-file-storage-doc-concurrency-failure-model`

The exhaustive state / race / failure model of the write path, **as implemented** — every claim below is
traceable to a function in `gears/file-storage/file-storage/src/`. Companion to [DESIGN.md](./DESIGN.md)
(§3.6 upload sequences and the auto-bind-on-finalize behavior, §4.5 signed URLs, §4.7 multipart worked
example) and [api.md](./api.md) (wire contract, `X-FS-Bound` / `bind_state`, `202 completing`). Where this
document and those disagree, the code cited here wins; file an issue.

- [1. Ground rules](#1-ground-rules)
- [2. State graph](#2-state-graph)
  - [2.1 Single-part upload](#21-single-part-upload)
  - [2.2 Multipart upload](#22-multipart-upload)
  - [2.3 Clock & timeout inventory](#23-clock--timeout-inventory)
- [3. Failure matrix](#3-failure-matrix)
- [4. Race catalog](#4-race-catalog)
- [5. Invariants](#5-invariants)
- [6. Traceability](#6-traceability)

## 1. Ground rules

Two properties shape everything below:

1. **No DB transaction or lock is ever held across network I/O.** Every state transition is either a
   single conditional `UPDATE ... WHERE <expected state>` (a CAS) or a short multi-statement transaction
   whose statements are all local DB writes. Backend I/O (S3 `PutObject`/`UploadPart`/
   `CompleteMultipartUpload`, local-fs writes, the read-back, the MIME-sniff read) always happens
   **between** transitions, never inside one. Consequence: a crashed participant leaves a *state*, never
   a held lock — recovery is always "observe the state, CAS forward".
2. **Every retry is safe by construction.** Idempotency is not best-effort: `publish_exclusive` refuses
   double-writes at the backend (`infra/backend/*::publish_exclusive`), finalize converges replays
   (`domain/service/write.rs::finalize_upload_by_token`), `complete` replays its persisted result
   (`domain/multipart_service.rs::replay_completed`), and the content pointer moves only via
   `FileRepo::bind_content_cas` (`infra/storage/repo/file_repo.rs`) — one atomic
   `UPDATE files SET content_id = :new WHERE file_id = :id AND content_id IS NULL / = :expected`
   (PRD `cpt-cf-file-storage-fr-conditional-requests`, §5.10 amendment).

## 2. State graph

### 2.1 Single-part upload

`POST /files` (default `bind: "auto"`, no `multipart` block or a plan that collapses to one part) → `PUT` →
done. Two requests total (DESIGN §3.6 amendment).

```text
 (none) ──create──► file row (content_id NULL) + version PENDING
                          │  client PUT bytes → sidecar
                          ▼
                    bytes streaming ──publish_exclusive──► blob published (immutable)
                          │  sidecar → control finalize callback
                          ▼
                    version AVAILABLE ──(bind_on_finalize claim, same tx)──► content_id CAS
                          │                                                    │
                          ▼                                                    ▼
                    PUT 200 + X-FS-Bound: true + ETag              PUT 200 + X-FS-Bound: conflict
                                                                   + X-FS-Current-ETag (version stays
                                                                     AVAILABLE, manually bindable)
```

| # | Transition | DB writes | Backend I/O | What is in flight / which clock bounds it |
|---|---|---|---|---|
| S1 | create | **One tx**: `files` insert (`content_id NULL`) + `file_versions` insert (`pending`) + custom metadata + audit + event outbox [+ idempotency row] — `Store::create_file_with_pending_version_and_event` (`infra/storage/store/files.rs`). Signed URL minted **before** the tx (pure CPU, `FileService::sign_url_with_bind`) | none | The `POST /files` HTTP request. Nothing long-running |
| S2 | bytes streaming | none | Sidecar streams chunks to the backend while hashing incrementally (`bin/sidecar.rs::upload`), enforcing `max_size` mid-stream (`413`) and `exact_size`/`expected_hash` at end-of-stream. Publish is **create-exclusive** (`publish_exclusive`) — a replay against an already-published path writes nothing | The client's `PUT` connection, for the whole stream. Bounded by the token's `exp` *at request start* (verification precedes streaming; `exp` is not re-checked mid-stream) and by `FS_SIDECAR_MAX_BODY_BYTES` (default 5 GiB) |
| S3 | finalize | **One tx**: version `pending → available` CAS (`VersionRepo::finalize` requires `status = 'pending'`) + audit + — when the verified token carries `bind_on_finalize` — the `content_id IS NULL` bind CAS + current-flag flip + bind audit + `file.content_updated` event (`Store::finalize_version` with `AutoBindOnFinalize`, `infra/storage/store/versions.rs`) | **Before** the tx: control plane re-reads the whole blob (`read_back_and_hash_streaming`, streamed) to independently recompute size/hash, plus MIME magic-byte validation on the leading 8 KiB (`infra/content/mime.rs`) | The client's `PUT` is **still open**: the sidecar answers only after the finalize callback returns. The callback is bounded by `FS_SIDECAR_FINALIZE_TIMEOUT_SECS` (default **10 s**, total) / `FS_SIDECAR_FINALIZE_CONNECT_TIMEOUT_SECS`, retried up to `CALLBACK_MAX_ATTEMPTS = 3` (100 ms apart) on transport errors only (`post_with_retry`, `bin/sidecar.rs`). ⚠ For very large single-part objects the read-back can exceed the 10 s budget — see F5 in the matrix |
| S4 | report to client | none | none | Sidecar copies the finalize response's `x-fs-bound`/`etag`/`x-fs-current-etag` headers verbatim onto its `200` (`uploaded_response`, `bin/sidecar.rs`); it never interprets them (ADR-0003 amendment) |

`bind: "manual"` (or `POST /files/{id}/versions`, which never mints the bind claim —
`infra/signed_url/mod.rs::Claims::bind_on_finalize`) stops after "version AVAILABLE"; the client issues the
classic `POST /files/{id}/bind` (`FileService::bind`, `If-Match` CAS).

### 2.2 Multipart upload

`POST /files` with the `multipart` block (plan ≥ 2 parts) → `PUT × N` → `POST complete`. N+2 requests.
The session is a lease-guarded state machine; `bind` happens inside `complete` for `auto_bind` sessions.

```text
 (none) ──create+plan──► file row + version PENDING + session IN_PROGRESS(auto_bind)
                              │ per part: (not uploaded) ──sidecar PUT──► written ──report-part──► REPORTED (row in multipart_upload_parts)
                              ▼ client POST complete
                    IN_PROGRESS ──acquire lease CAS──► COMPLETING(lease_owner, lease_until)
                          losers ──► 202 {state: completing}                │ detached assembly task
                                                                            ▼
                                                       CompleteMultipartUpload + 8 KiB MIME sniff
                                                                            │
                                                       finalize tx (available + bind CAS) ── on error: release-lease CAS ──► IN_PROGRESS
                                                                            ▼
                                                       COMPLETED(complete_result JSON)   [ABORTED via client abort / cleanup sweep]
```

| # | Transition | DB writes | Backend I/O | In flight / clocks |
|---|---|---|---|---|
| M1 | create+plan | Tx A: file row only (`Store::create_file_with_event` — **no** single-part pending version, so no presign orphan); then `file_versions` pending insert; then session insert (`MultipartRepo::create`, `auto_bind` recorded). Session-insert failure triggers best-effort compensation (`compensate_failed_session_create`) | `StorageBackend::initiate_multipart` (S3 `CreateMultipartUpload`) between the version insert and the session insert | The `POST /files` request. Per-part URLs share one `exp` (`default_url_ttl_secs`); the session gets its own `expires_at` (`multipart_session_ttl_secs`) |
| M2 | part upload | none by the sidecar; the **control plane** upserts the part row on the token-authenticated report callback (`MultipartStore::upsert_multipart_part` — single upsert) | Sidecar streams the part (`upload_part` / S3 `UploadPart`), enforcing the token's **exact** `size` claim, hashing on the fly | The part's `PUT` connection; report callback bounded like finalize (10 s / 3 attempts). Part URL `exp` bounds *starting* an upload; the session `expires_at` bounds the whole endeavour (two-clock model, DESIGN §4.7 Phase C) |
| M3 | acquire lease | **Single CAS**: `state='completing', lease_until=now+K, lease_owner=:me WHERE upload_id=:id AND (state='in_progress' OR (state='completing' AND lease_until < now))` — `MultipartRepo::acquire_complete_lease`. One statement covers fresh acquire **and** dead-owner takeover | none | Read-only pre-flight (missing-parts diff, size, policy) runs **before** the CAS, so a deterministic rejection never occupies the lease. `K = multipart_complete_lease_secs` (default 120 s) |
| M4 | assembly | none | Winner's **detached task** (`tokio::spawn` in `complete_multipart_upload`; the HTTP handler awaits its `JoinHandle`, but a dropped request future cannot cancel the work): S3 `CompleteMultipartUpload` (manifest + root folded from reported part rows — **no re-read**, ADR-0006), then one ~8 KiB ranged `get_range` for MIME sniffing. Takeover recovery: if the backend handle was already consumed but the assembled object exists, `(manifest, root)` are rebuilt locally from the part rows (`assemble_and_finish_inner`) | The client's `complete` request is open but expendable (F5-safe). The lease clock bounds how long the state stays `completing` unobserved |
| M5 | finalize (+bind) | **One tx**: version `pending → available` + hash/manifest row + audit + (`session.auto_bind`) the bind CAS against the `content_id` validated by the endpoint's `If-Match` + bind audit + event (`Store::finalize_version`) | none | — |
| M6 | finish | **One tx**: CAS `completing → completed` + persist `complete_result` JSON (`StoredCompleteResult`) + audit — `MultipartRepo::finish_complete`. On a failed assembly instead: release-lease CAS `completing → in_progress WHERE lease_owner = :me` (`release_complete_lease`), so the next `complete` retries immediately | none | — |

### 2.3 Clock & timeout inventory

| Clock | Default | Bounds | Source |
|---|---|---|---|
| token `exp` | 15 min (`default_url_ttl_secs`, hard cap `max_url_ttl_secs` ≤ 7 d) | starting any signed operation (PUT/part/GET); *not* an in-flight stream | `config.rs`; DESIGN §4.5 |
| session `expires_at` | 24 h (`multipart_session_ttl_secs`) | the whole multipart endeavour incl. resume; `complete` rejects past it | `config.rs`; DESIGN §4.7 |
| completion lease | 120 s (`multipart_complete_lease_secs`) | one `complete`'s exclusive assembly window; takeover after | `config.rs`, `multipart_service.rs` |
| finalize/report callback | 10 s total, 3 attempts × 100 ms (transport errors only) | sidecar → control callbacks | `FS_SIDECAR_FINALIZE_TIMEOUT_SECS`, `bin/sidecar.rs` |
| cleanup grace | `orphan_grace_secs` | when abandoned `pending` versions / expired sessions become sweepable | `domain/cleanup.rs` |

## 3. Failure matrix

"Cleanup" = the P2 cleanup engine (`domain/cleanup.rs`,
`cpt-cf-file-storage-fr-orphan-reconciliation`): sweeps `pending` versions older than
`orphan_grace_secs` (+ their blobs, + zero-version orphan `files` rows), and aborts expired sessions —
`in_progress` past `expires_at`, **and** `completing` past `expires_at` whose lease has also expired
(`MultipartRepo::list_expired`, `abort_expired_completing` — a live lease is never reaped mid-flight).

| State when the failure hits | Client dies | Control plane dies | Sidecar dies | Backend (S3) down | Network drops after the op, before the response |
|---|---|---|---|---|---|
| F1: after create, before any bytes | `pending` version (+ file row) idles; **garbage**: pending version, swept by cleanup after grace; file row swept once versionless. Retry `POST /files` with the same `idempotency_key` replays the same ticket (fresh-signed URL) instead of a second file | create is one tx — either fully committed or nothing. Client retry: idempotency replay (committed) or plain re-create (not) | n/a (not involved yet) | n/a | same as "control plane dies": the tx committed; an idempotency-key retry converges on it, a keyless retry makes a second file (first becomes cleanup fodder) |
| F2: mid-stream (single-part PUT / part PUT) | Sidecar sees the broken stream, never publishes / never reports; best-effort deletes the partial object. Version stays `pending` (single-part → cleanup fodder; multipart → the part is simply "missing", resume via `GET status`) | stream is sidecar↔backend; unaffected until finalize/report — see F5 | client gets a connection error; nothing published (create-exclusive publish is all-or-nothing per object; an S3 part that never completed is invisible). Retry the PUT / the part with the same token (until `exp`) or a fresh resume URL | sidecar's backend write fails → 5xx to client; partial object best-effort deleted; retry later. If S3 stays down: version stays `pending` → cleanup | n/a (the op did not finish) |
| F3: blob published, finalize callback never sent (sidecar crashed in between) | — | — | Version stays `pending` with real bytes at `backend_path` — **garbage** until: (a) the client retries the `PUT` (replay: `publish_exclusive` refuses the write, finalize is still attempted with the retry's digest and converges — `bin/sidecar.rs::upload`'s `!created` decision table), or (b) cleanup sweeps blob+row after grace | — | — |
| F4: finalize succeeded, response lost (client saw no 200 / sidecar died before answering) | Version is `available` (+bound if auto). Client retries the `PUT`: publish refused (`created: false`), finalize replays and **converges idempotently** — same size/hash against the stored version → same `X-FS-Bound`/`ETag` headers, never a 409 (`finalize_upload_by_token`'s already-`available` branch; mismatched bytes still rejected via hash compare) | finalize tx committed or not — atomic. Not committed: F3. Committed: this cell | same convergence via PUT replay | n/a | this **is** that case |
| F5: finalize in progress, control plane dies (or the 10 s callback timeout fires mid-read-back) | — | Handler future dropped → tx never ran → version `pending`, blob published = F3. Sidecar retries the callback up to 3× (transport/timeout), then answers `502` (fresh publish) or `409` (replay) — the client re-`PUT`s later and converges. ⚠ a single-part object whose read-back reliably exceeds 10 s will loop here — raise `FS_SIDECAR_FINALIZE_TIMEOUT_SECS` or use multipart (its complete has no full read-back, ADR-0006) | — | read-back fails → finalize 5xx → F3 recovery | — |
| F6: session `in_progress`, some parts reported, client dies | Durable part rows survive (`multipart_upload_parts`). Anyone with `{file_id, upload_id}` resumes: `GET /files/{id}/multipart/{upload_id}` → `received` + `missing` with fresh URLs (`introspect_multipart_upload`) — until `expires_at`, after which cleanup aborts the session (backend `AbortMultipartUpload`, part rows + pending version deleted) and resume gets `404` | part reports are lost only for parts whose callback didn't land — those parts read as "missing", re-uploadable. No corruption | in-flight parts lost (→ missing), reported parts durable | parts can't be written until S3 returns; session survives up to `expires_at` | a part written but its report lost: part reads as missing; re-uploading it hits S3 `UploadPart` idempotently (same part number overwrites) and re-reports |
| F7: `completing`, winner alive | The **detached task keeps running** (client disconnect ≠ cancellation); result is persisted. Any later `complete` (F5-refresh, other tab) gets `202` while the lease is live, then the replayed result | see F8 | sidecar not involved in complete | assembly fails → release-lease CAS → back to `in_progress`, error returned; immediate retry possible | response lost after `completed`: re-`complete` replays `complete_result` verbatim — idempotent by design |
| F8: `completing`, winner (control-plane node) dies mid-assembly | — | State stuck at `completing` until `lease_until` passes; then the next `complete` **takes over** via the same acquire CAS: version already `available` → finish-only fast path; backend handle consumed but object assembled → local manifest rebuild from part rows; otherwise full re-assembly (`assemble_and_finish_inner`). If nobody ever calls `complete` again: once `expires_at` *and* the lease have both passed, cleanup aborts the session (this doc's model; `list_expired`) | — | takeover inherits the same S3-down behaviour as F7 | — |
| F9: `completed`, bind was `conflict` | Nothing is lost: the version is `available`; the response (or its idempotent replay) carries `current_etag`; one manual `POST bind` with that `If-Match` makes it live — **no re-upload** (PRD §5.10) | — | — | — | — |

## 4. Race catalog

1. **Two concurrent `complete`s.** Both pass the read-only pre-flight; exactly one wins the lease CAS
   (single conditional UPDATE — the DB serializes it). The loser immediately re-reads the session:
   `completing` → `202 {state: "completing", retry_after_secs}` (+`Retry-After`); `completed` → replay.
   Polling = re-issuing the same idempotent call. No double assembly on the success path.
2. **Takeover after a dead lease owner.** The acquire CAS's second arm
   (`state='completing' AND lease_until < now`) makes takeover the *same* operation as acquisition — no
   separate recovery protocol. The taker distrusts the dead owner's progress and re-derives it from
   durable state: version row status, backend object existence, part rows (§2.2 M4). A slow-but-alive
   original owner that finishes assembly after losing its lease cannot corrupt anything: its
   `finish_complete` CAS (`WHERE state='completing'`) still succeeds only if no one else finished first,
   and `VersionRepo::finalize`'s own `status='pending'` CAS makes the version flip once-only; a lost
   finish converges via `replay_completed` (`finish_session`'s not-finished branch).
3. **PUT replay on the same token** (until `exp`). Unpublished path: lands as an ordinary write.
   Published path: `publish_exclusive` refuses (**bytes never mutate in place**); finalize still runs
   with the replay's digest and either converges (same bytes) or is rejected by the stored-hash /
   read-back comparison (different bytes) — the live object is never re-labelled. DESIGN §4.5
   "Bound by the signature" + `bin/sidecar.rs::upload` decision table.
4. **Two upload tokens for one new file** (double `POST /files`, or create-retry without an idempotency
   key). Two files in the retry case (see F1). Within one file (create + `POST /files/{id}/versions`,
   or an idempotency replay racing the original PUT): both finalize; the first `bind_on_finalize` wins
   the `content_id IS NULL` CAS, the second gets `X-FS-Bound: conflict` + `X-FS-Current-ETag` — its
   version stays `available` and manually bindable. Nothing is silently overwritten.
5. **`complete` vs concurrent manual `bind`.** `complete` validates its optional `If-Match` against the
   file row it read, then passes that exact `content_id` as the bind CAS expectation into the finalize
   tx. A manual `bind` (or another complete) landing in between moves the pointer → the embedded CAS
   loses → `bind_state: "conflict"` + `current_etag`; the upload itself still succeeds.
6. **Concurrent content replacement in general.** All pointer movement funnels through
   `bind_content_cas`; a loser re-reads the current ETag and re-binds the **already-uploaded**
   `version_id` — the retry never re-uploads bytes (PRD §5.10; DESIGN §3.6 conflict note).
7. **`POST /files` retry.** With `idempotency_key`: replay of the stored ticket, bound to the same
   subject and request hash, URL re-minted under the *current* policy (`create.rs`). Without: a second
   file. With the `multipart` block the key is rejected (`400`) — multipart create-retry recovery is
   client-side (`{file_id, upload_id}` persistence + introspect), a known gap noted in api.md.
8. **F5 / page reload — the two-clock model.** Per-part URL `exp` (minutes) only forces re-presigning
   via introspect; session `expires_at` (24 h) bounds actual progress loss (DESIGN §4.7 Phases B–C).
   A reload racing an in-flight `complete` re-issues it: `202` while the (disconnect-immune, §2.2 M4)
   detached task runs, then the recorded result.

## 5. Invariants

- **Pointer moves only by CAS.** Every `files.content_id` change — manual `bind`, auto-bind on finalize,
  bind inside `complete` — is the same single conditional UPDATE (`bind_content_cas`). No path writes the
  pointer unconditionally.
- **At most one effective assembly per success.** The lease CAS serializes completers; a takeover
  re-derives progress from durable state and reuses the already-assembled object where possible; the
  version's `pending → available` flip is itself a once-only CAS.
- **Every retry is safe**: create (idempotency key), PUT (create-exclusive publish + convergent
  finalize), part PUT (exact-size claim + upsert), report/finalize callbacks (CAS-guarded),
  `complete` (persisted-result replay), `bind` (CAS + re-readable ETag).
- **A lost bind CAS loses nothing.** The bytes are uploaded, the version is `available`, the response
  names the current ETag; one manual re-bind finishes the job.
- **Bytes never mutate in place.** Backend objects are immutable per `(file_id, version_id)`
  (`publish_exclusive`; new content = new version + pointer swap, DESIGN §3.1/§3.6).
- **Garbage is bounded and owned.** Every failure leaves at most: a `pending` version (+blob), an
  unbound `available` version, orphan S3 parts of an aborted handle, or a lease-expired `completing`
  session — each enumerated in §3 with its reaper (cleanup engine) or its converging retry.

## 6. Traceability

- **Code**: `domain/multipart_service.rs` (`complete_multipart_upload`, `assemble_and_finish*`,
  `replay_completed`, `finish_session`), `domain/service/write.rs` (`finalize_upload_by_token`,
  `bind`), `domain/service/create.rs`, `infra/storage/repo/multipart_repo.rs` (lease CASes),
  `infra/storage/repo/file_repo.rs::bind_content_cas`, `infra/storage/store/versions.rs::finalize_version`,
  `bin/sidecar.rs` (`upload`, `post_with_retry`, `uploaded_response`), `domain/cleanup.rs`.
- **PRD**: `cpt-cf-file-storage-fr-conditional-requests` (§5.10 + amendment),
  `cpt-cf-file-storage-fr-multipart-upload`, `cpt-cf-file-storage-fr-upload-idempotency`,
  `cpt-cf-file-storage-fr-orphan-reconciliation`.
- **DESIGN**: §3.6 (sequences + auto-bind amendment), §4.5 (signed URLs), §4.7 (worked example, two-clock
  resume). **ADRs**: 0003 (sidecar data plane + bind amendment), 0004 (token transport), 0006
  (content-hash modes; no-re-read complete).
