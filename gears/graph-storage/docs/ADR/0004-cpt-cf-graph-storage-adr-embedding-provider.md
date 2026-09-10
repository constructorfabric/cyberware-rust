---
status: accepted
date: 2026-08-13
decision-makers: Graph Storage design review
---

# ADR-0004: Embeddings come from a pluggable provider with an in-process ONNX default

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [A. In-process ONNX only](#a-in-process-onnx-only)
  - [B. Remote embedding only](#b-remote-embedding-only)
  - [C. Caller-supplied vectors only](#c-caller-supplied-vectors-only)
  - [D. Pluggable provider with in-process default](#d-pluggable-provider-with-in-process-default)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-graph-storage-adr-embedding-provider`

## Context and Problem Statement

The prototype embedded node search text and content chunks with `sentence-transformers/all-MiniLM-L6-v2` (384 dimensions, L2-normalized) via PyTorch — a Python-only stack that cannot ship inside a Rust gear. Vector search quality depends on ingest-time and query-time embeddings coming from the same model, and the storage schema fixes the vector dimension. The gear must decide where embeddings are computed and how the model choice is bound to a deployment.

## Decision Drivers

- `cpt-cf-graph-storage-fr-embedding-pipeline` requires batched embedding at ingest and query time with a per-request skip.
- `cpt-cf-graph-storage-fr-embedding-dim-guard` requires the provider's dimension to be verified against the schema at readiness and per batch — the prototype documented dimension mismatch as a real failure mode.
- PyTorch/sentence-transformers is not embeddable in Rust; the equivalent MiniLM-family models are published in ONNX form and runnable via ONNX Runtime bindings (the fastembed/ort ecosystem).
- Some deployments will prefer centralized inference (GPU pools, platform LLM gateway) over per-gear model loading; the gear should not hard-code either topology.
- Deterministic CI needs an embedding path with no model download and no numeric drift.
- The platform plugin pattern (GTS-registered plugin instances behind a client trait) is the established mechanism for exactly this kind of deployment-selected backend.

## Considered Options

- A. In-process ONNX embedding only, model bundled with the gear
- B. Remote embedding only, via the platform LLM gateway or an external inference endpoint
- C. Caller-supplied vectors only: producers embed, the gear just stores
- D. Pluggable embedding provider behind a gear plugin contract — in-process ONNX default plugin, remote-endpoint plugin as the alternative, deterministic fake for tests

## Decision Outcome

Chosen option: "D. Pluggable embedding provider", because it ports the prototype's proven encoder-protocol design onto the platform's plugin pattern: one `EmbeddingProvider` contract (batch texts to vectors; declared embedding-space identity and dimension), selected per deployment. The default plugin runs an ONNX MiniLM-class sentence-embedding model in-process (matching the prototype's 384-dimension, normalized-vector behavior); a second plugin calls a remote inference endpoint; a deterministic hash-based fake serves CI.

Providers declare a full **embedding-space identity**, not merely a dimension: the exact model artifact (name plus version or content hash), the tokenizer artifact, and the preprocessing, pooling, and normalization configuration. Dimension alone is not a sufficient guard — a different tokenizer, pooling rule, or weight set can still emit 384-dimensional normalized vectors that are mutually incomparable. The identity under which stored vectors were produced is recorded durably alongside them; readiness compares the active provider's identity (and dimension) against that record, and on mismatch fails readiness and blocks vector search until re-embedding completes. Query-time embedding always uses the same active provider as ingest.

### Consequences

- The gear defines and publishes the provider plugin contract (`cpt-cf-graph-storage-contract-embedding-provider`) with model identity, dimension, and batch semantics; plugins register as GTS plugin instances.
- Changing the active model is an operational event, not a request-level option, and it has a defined resumable recovery lifecycle: `requested -> scanning -> embedding -> validating -> cutover -> complete` (with `failed` reachable from any stage and resumable). The migration is operator-triggered through an administrative API (automatic switching is deliberately excluded — see the PRD open question on model governance), runs deployment-wide with per-tenant progress, checkpoints durably so retries resume rather than restart, and is idempotent per node and chunk. During backfill both embedding epochs coexist: writes continue under the *old* identity, similarity search serves old-identity vectors, and new-identity vectors accumulate invisibly. Cutover is a single atomic switch of the durable embedding-space identity, after which old-identity vectors are stale and re-embedded on demand or by a follow-up pass; readiness and vector search are restored only when the new identity is in force and no vectors remain under the old one. Cancellation rolls forward to the last checkpoint and leaves the old identity active.
- The in-process default keeps small deployments dependency-free (no inference service), at the cost of bundling ONNX runtime and model weights with the gear image.
- Mixed-model graphs are structurally prevented: one provider configuration per deployment per vector column lifetime, and the recorded embedding-space identity turns any drift — same-dimension model swaps included — into a readiness failure with vector search blocked, instead of silent quality loss.
- Embedding cost stays out of the ingest critical path decision: producers may skip embedding per request and rely on later re-embedding passes; a preserved vector remains valid only while its recorded embedding-input hash is unchanged (stale vectors are excluded from similarity search until re-embedded).
- Remote providers are governed data egress, not ordinary plugin calls: node search text, content chunks, and user query text leave the deployment. A default-deny per-tenant egress policy gates every remote embedding call — approved vendor/endpoint/model/region, permitted data classes and vectorized fields, separate rules for ingest text, chunks, and query text, byte/token limits with minimization, provider retention/deletion/no-training requirements, and metadata-only audit evidence. A denied tenant fails or explicitly skips per the API contract; the gear never silently switches models, which would break the single-vector-space invariant.

### Confirmation

- Contract tests run all three plugins (ONNX, remote via mock server, fake) against the provider contract, including batch behavior and dimension declaration.
- Readiness tests cover dimension mismatch (provider vs. schema), embedding-space identity mismatch at identical dimension (a different model producing 384-dim vectors must fail readiness and block vector search), and provider unavailability.
- An end-to-end test verifies query-time and ingest-time embeddings agree (a document ingested and then queried with its own text ranks first in the vector arm).

## Pros and Cons of the Options

### A. In-process ONNX only

The gear always loads and runs the model itself.

- Good, because deployments are self-contained with zero external inference dependencies.
- Good, because latency is local and predictable.
- Bad, because GPU-pooled or centrally governed model serving is impossible, forcing every gear replica to hold model memory.
- Bad, because model upgrades ship as gear releases even where operators want independent model lifecycle.

### B. Remote embedding only

All embedding calls go to a platform inference endpoint.

- Good, because model governance, scaling, and hardware live in one central place.
- Good, because the gear image stays small.
- Bad, because small and air-gapped deployments must stand up an inference service to use any vector feature.
- Bad, because ingest batches become chatty network fan-outs with a new availability dependency on the hot write path.

### C. Caller-supplied vectors only

Producers embed with whatever model they choose and send vectors.

- Good, because the gear needs no model runtime at all.
- Bad, because nothing enforces that all producers and the query side use the same model — mixed-model vector spaces silently break similarity ranking, and the gear cannot embed query text at all without a model.
- Bad, because every producer duplicates embedding infrastructure.

### D. Pluggable provider with in-process default

Provider contract; ONNX default plugin; remote plugin; deterministic fake.

- Good, because deployment topology (local vs. centralized inference) is a configuration choice, not an architecture change.
- Good, because the deterministic fake gives CI reproducibility, carried over from the prototype's proven practice.
- Good, because the contract pins model identity and dimension where the dimension guard can verify them.
- Neutral, because two production plugins must be maintained from day one.
- Bad, because the plugin indirection adds contract-versioning overhead compared to a hard-coded encoder.

## More Information

The prototype's encoder protocol (name, dimension, batch embed), its normalized 384-dimension MiniLM vectors with cosine HNSW indexes, its `COALESCE`-preserving non-embedding upserts, and its fake encoder for CI are the direct ancestors of this design. Model standardization and upgrade governance remain open in PRD § 13.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses:

- `cpt-cf-graph-storage-fr-embedding-pipeline` — the provider contract is the pipeline's backend
- `cpt-cf-graph-storage-fr-embedding-dim-guard` — declared dimension enables readiness and batch verification
- `cpt-cf-graph-storage-fr-vector-search` — query-time embedding uses the same active provider as ingest
- `cpt-cf-graph-storage-contract-embedding-provider` — this ADR mandates that contract
