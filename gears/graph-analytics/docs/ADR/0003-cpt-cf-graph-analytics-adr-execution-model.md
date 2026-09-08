---
status: accepted
date: 2026-08-24
decision-makers: Graph Analytics design review
---

# ADR-0003: Jobs are durable, leased, memory-reserved, and admitted through a bounded queue

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Migration to the platform Task Engine](#migration-to-the-platform-task-engine)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [A. Synchronous computation with a request timeout](#a-synchronous-computation-with-a-request-timeout)
  - [B. In-memory job registry with a worker pool](#b-in-memory-job-registry-with-a-worker-pool)
  - [C. Durable job table with leases, reservations and a bounded queue](#c-durable-job-table-with-leases-reservations-and-a-bounded-queue)
  - [D. An external queue or scheduler](#d-an-external-queue-or-scheduler)

<!-- /toc -->

**ID**: `cpt-cf-graph-analytics-adr-execution-model`

## Context and Problem Statement

A metrics job over a million-node graph runs for minutes and holds gigabytes.
That single fact rules out answering synchronously, and everything else follows
from it: a computation that outlives its request needs an identity, an owner and
a terminal state that survives a restart; a computation that reserves gigabytes
needs an admission decision made before allocation rather than during it; and a
gear whose most expensive operation is also its most retried one needs
deduplication, because the natural client behaviour — poll, time out, resubmit —
otherwise multiplies exactly the wrong thing.

Isolating analytics into its own gear
([`cpt-cf-graph-analytics-adr-own-gear-boundary`](./0002-cpt-cf-graph-analytics-adr-own-gear-boundary.md))
bounds the blast radius to this gear, but does not decide how work is admitted,
scheduled, or recovered inside it. That is what this ADR settles.

## Decision Drivers

- A job outliving gateway timeouts means the HTTP response cannot be the result,
  so job state exists whether or not it is designed deliberately.
- Per-tenant concurrency limits cannot bound the sum across tenants; without a
  process-wide bound, N tenants each within their limit still exhaust the host.
- Memory is reserved in large, long-lived blocks, so admission has to reason
  about an estimate before allocation. Discovering the ceiling by being killed is
  not an admission policy.
- A restart mid-computation must not lose the job identity a client is holding,
  and must not leave a job `running` forever with no worker behind it.
- Recovery creates a race by construction: a reclaimed job may have a previous
  worker still alive and about to write. Something must make the stale write
  lose deterministically.
- Cancellation must actually free the reservation, or a cancelled job leaks the
  budget it held and the pool degrades over the process lifetime.
- The gear already depends on PostgreSQL for the topology read; a second
  infrastructure dependency for queueing would be a new operational surface for
  a queue depth measured in tens.

## Considered Options

- A. Synchronous computation with a request timeout
- B. In-memory job registry with a worker pool
- C. Durable job table with leases, reservations and a bounded queue
- D. An external queue or scheduler

## Decision Outcome

Chosen option: "C. Durable job table with leases, reservations and a bounded
queue", because every one of the drivers above is a durability or an admission
problem, and both are solved by state the gear already has a transactional store
for. The elements, each earning its place:

1. **A durable job table** keyed by (tenant, job id), carrying the ownership
   tuple, the admitted graph revision, the deadline, the terminal error category
   and reason, and a reference to the published result. An accepted identifier
   survives restart, and terminal transitions — including the race between
   cancellation and publication — are single atomic updates rather than
   read-modify-write sequences.
2. **Leases with a fencing epoch.** A running job is claimed by a worker for a
   bounded time with a heartbeat. An expired lease is reclaimable, and the
   reclaim increments the epoch, so a late write from the superseded attempt is
   rejected on its epoch rather than overwriting the reclaiming worker's result.
   Lease recovery completes before workers report ready, so the gear never
   accepts new work while abandoned jobs are still unresolved.
3. **Estimate-and-reserve against a process-wide pool.** Each job's peak is
   estimated from node and edge counts plus key sizes and reserved at start;
   allocation is tracked during the run and a job exceeding its reservation is
   terminated rather than the process. The reservation is released on success,
   failure, cancellation and lease expiry alike — the last one is the case that
   is easy to miss and the one that leaks.
4. **A bounded queue with per-tenant fairness.** A job that cannot reserve queues
   rather than starting; a full queue is rejected with `resource_exhausted` and a
   retry hint. Per-tenant running and queued limits keep one tenant from filling
   the queue.
5. **Deduplication on full job identity** — (tenant, graph revision, metric,
   parameters, authorization-scope identity, contract version). A duplicate
   submission joins the in-flight job instead of starting a second one, and a job
   superseded by a newer revision is cancelled cooperatively.
6. **Conditional publication.** A result is published only if the graph revision
   has not moved during computation. A long job over a graph that has since
   changed reports superseded and writes nothing, rather than publishing a result
   for a state that no longer exists.

Rejections are classified by cause rather than by the fact that a limit was
involved: a value outside a documented hard range is `out_of_range`, an
internally inconsistent request is `invalid_argument`, and only transient queue,
concurrency or memory pressure is `resource_exhausted` with a retry hint.
Termination by time or cancellation is `deadline_exceeded` or `cancelled`.

### Migration to the platform Task Engine

The [Task Engine](https://github.com/constructorfabric/gears-rust/pull/4545) is
specifying exactly the control plane elements 1, 2 and 4 describe — durable task
lifecycle, atomic claim with lease and heartbeat, priority-ordered dispatch with
queue and per-type concurrency limits, retry and dead-letter handling, event
history, and a REST surface so a UI can schedule, pause, observe and cancel.
Building a second one here and keeping it is waste, so the decision above is
sequenced rather than permanent: this gear implements its own job table first,
because the Task Engine is a PRD in review with no DESIGN or implementation yet
and analytics cannot wait on it, and moves the control plane there once it
lands.

What transfers: the job row's lifecycle and terminal states, claim and lease,
heartbeat and restart reclamation, queue admission and per-tenant fairness,
cancellation signalling, retention of job history, and the management API. This
gear registers its metric-job types as GTS task types and implements workers.

What stays here regardless, because it is about graphs rather than about tasks:

| Element | Why it does not move |
|---|---|
| Estimate-and-reserve against a process-wide **memory** pool (3) | A generic engine bounds *how many* tasks run, not how many gigabytes they hold. An analytics job's peak is a function of node and edge counts, knowable only here, and the failure it prevents — the process being the wrong size before an allocation fails — is not a scheduling failure |
| Revision supersession inside deduplication (5) | The Task Engine's idempotency key is caller-supplied and returns the existing task. Ours is derived from graph state, and a *newer* graph revision must **cancel** an in-flight job rather than join it. That is a domain predicate on task identity, not a duplicate-submission rule |
| Conditional publication (6) and the metrics cache | The result is a graph artifact keyed by `(source_epoch, graph_revision, metric, parameters, contract version)`, and it is bigger than a task payload should be — the Task Engine caps a task's aggregate payload at 64 KB and refers larger artifacts out by URI, while `metrics_max_entry_bytes` is 4 MiB |

Two questions the Task Engine's contract has to answer before the control plane
can move, both raised on that PR:

1. **Is the lease fenced?** Claim, heartbeat and reassignment are specified;
   what is not is whether a reclaimed task carries a monotonic token that makes
   the superseded worker's *write* fail. Without one, reassignment prevents
   double-dispatch but not a late write landing after a reclaim — and for this
   gear that write is a cache publication.
2. **How does a terminal transition compose with a write in another database?**
   Publication here is one transaction: the cache row, the result reference and
   the `succeeded` transition commit together, because any split leaves a cache
   row nothing references or a `succeeded` job whose result resolves to nothing.
   With the task row in the Task Engine's store that becomes a two-store commit.
   The likely answer is that this gear keeps a local terminal record as the
   authority for publication and reports outcome to the Task Engine, but that
   shape needs agreeing rather than assuming.

Until both are answered, the local job table stays the authority; the Task
Engine adoption is tracked as a dependency rather than as a decision already
taken.

### Consequences

- The job table is this gear's own, in the graph-storage database but written
  only here. graph-storage neither reads nor writes it, so single-writer holds
  per table.
- Every long-running operation gains a state machine to test, and its concurrent
  transitions are the part most likely to be wrong — so they are named in the
  acceptance criteria explicitly rather than left to unit tests of happy paths.
- Multi-instance deployment is not supported in v1: the memory pool is
  process-wide, so two instances would each admit against their own pool and
  jointly overcommit the host. Recorded as an open question rather than designed
  around.
- Deduplication makes the authorization-scope identity part of job identity even
  though v1 rejects constrained scopes. That is deliberate: it keeps the cache
  and dedup keys correct if resource-scoped analytics is added, rather than
  requiring a migration of both.
- Memory estimation is now a correctness-relevant function, not a heuristic. If
  it under-estimates systematically the pool over-commits, which is why
  allocation is also tracked at run time as a backstop.

### Confirmation

- Restart mid-computation: the job identifier remains valid, the job is reclaimed
  and completes or fails terminally — it never remains `running` with no worker.
- Lease expiry and reclaim, with the superseded worker's late write asserted to
  be rejected on its epoch.
- The cancellation-versus-publication race asserted from both orderings.
- A cancelled and a lease-expired job each assert that reserved memory returns to
  the pool, checked against the pool gauge rather than inferred.
- Queue saturation returns `resource_exhausted` with a retry hint, and never
  admits beyond the pool.
- A duplicate submission joins the in-flight job: exactly one computation runs,
  and both callers receive the same job identifier.
- A revision change mid-computation results in no publication and a superseded
  terminal state.

## Pros and Cons of the Options

### A. Synchronous computation with a request timeout

- Good, because there is no job state at all, and the API is trivial.
- Bad, because a multi-minute computation cannot be answered inside a gateway timeout, so the operation would simply be unavailable at the sizes it matters for.
- Bad, because a timed-out request leaves the computation running with nothing tracking it, which is the worst of both models.

### B. In-memory job registry with a worker pool

- Good, because it is simple and needs no schema.
- Good, because it covers the timeout problem: the client polls an in-process registry.
- Bad, because a restart loses every job identifier a client is holding, turning a routine deploy into a client-visible failure.
- Bad, because there is nothing to reclaim from — an abandoned computation leaves no trace to recover, so the memory it reserved is only recovered by process exit.

### C. Durable job table with leases, reservations and a bounded queue

- Good, because durability, admission, fairness and recovery are all solved with a store the gear already depends on.
- Good, because the fencing epoch makes the recovery race deterministic instead of unlikely.
- Good, because reservations make the memory bound an admission decision rather than an outcome.
- Bad, because it is the most machinery of the three viable options, and the concurrent transitions need real tests.
- Bad, because the process-wide pool ties the design to a single instance in v1.

### D. An external queue or scheduler

- Good, because queueing, retries and fairness are someone else's implementation.
- Good, because multi-instance scheduling would come for free.
- Bad, because it adds an infrastructure dependency and an operational surface for a queue whose depth is measured in tens.
- Bad, because the interesting state is not "a message was delivered" but "this tenant's revision-R PageRank is being computed by that worker under this lease" — which would end up in a table anyway, next to the queue rather than instead of it.
