# Rule module: Async, Concurrency and Performance

This file is a rule module. Exactly one agent reads it, and that agent reads no other
module. See `docs/toolkit-pr-review/agents/subject.md` for how that agent works,
`docs/toolkit-pr-review/review-conventions.md` for severity and marker
conventions, and `docs/toolkit-pr-review/comment-style.md` for how a finding is worded.

## Scope of this module

Apply every rule in this module to every changed file in the PR.

## Check IDs to Apply

Apply **only** these specific check IDs. Each rule's `**Severity**` is the value to put in the
finding; do not infer it from the example in the Output Contract.

### 1. RUST-ASYNC-001 — Async Code Is Runtime-Safe
**Severity**: CRITICAL

- [HIGH] No blocking I/O or long CPU-bound work on an executor thread without offloading: `std::thread::sleep`, `File::read`, `TcpStream::read`, CPU-heavy work with no `spawn_blocking`
- `.await` while holding a lock, unless the design explicitly requires and justifies it. Never prescribe `tokio::sync::Mutex` as the fix
  why: the defect is the cross-`await` critical section itself. Switching mutex type makes the
       hold legal without removing it, and contradicts the preference for ownership transfer
       and message passing in RUST-CONC-001.
- **No timeout on an operation that can hang indefinitely**: HTTP, database, gRPC, a channel receive with no deadline
  why: any such call needs `tokio::time::timeout` or an equivalent bound. Absence of a timeout
       is the finding; `timeout` is not merely something that causes cancellation.
- [HIGH] **Retries that are not bounded and observable.** A retry loop needs a cap, backoff with jitter, and logging. Flag loops that can retry forever, retry without jitter, or retry silently
- [HIGH] **Background tasks with no lifecycle control or error handling**, in particular a `tokio::spawn` whose `JoinHandle` is dropped
  why: a spawned task needs cancellation, failure signalling and a shutdown path. A dropped
       handle leaves it detached, unsupervised and unstoppable.
- A function holding partial or shared state across `.await` with no auditable cancel-safety story
  why: the question is what happens when the future is dropped mid-await via `select!` or a
       timeout.
- [HIGH] `Drop` cannot `.await`. Audit `Drop` impls on async-held resources (transactions, connections, guards) for cleanup that actually needs an async call; it must be explicit, not assumed to run via `Drop`
- [HIGH] CPU-bound async loops with no periodic `tokio::task::yield_now()`, starving other tasks on the same executor thread
- [MEDIUM] An async fn reachable from `select!`, `timeout`, or an abortable task with no `// cancel-safe:` or `// NOT cancel-safe:` comment
  why: "all awaits are idempotent" is not a valid reason, and per-call semantics differ —
       `read` is cancel-safe, `read_exact` is not.
- A lock guard that escapes: returned from a helper, stored in a struct field, or produced by `MutexGuard::map`. `Enforcement: clippy await_holding_lock, await_holding_refcell_ref (deny)` for the direct shape only
  why: the lint catches a guard held across an `await` in place. The escaping shapes slip past
       it, so those are the ones you are looking for.

### 2. RUST-CONC-001 — Shared State and Concurrency Are Well Designed
**Severity**: HIGH

- Shared mutable state is minimized
- **Lock scope is small and intentional.** A critical section that spans unrelated work, or a guard held far longer than the data it protects is read, is a finding
- **Synchronization is not broader than necessary** — one lock protecting several independent fields should be split
- The chosen primitive matches the workload: channels, atomics, `Mutex`, `RwLock`
- No obvious deadlock **or starvation** risk: lock ordering, nested locks, writer starvation under `RwLock`, a hot lock monopolized by one task
- **Concurrency assumptions are visible in the code**, not implicit in the author's head
- Channel misuse: sending on a closed channel, dropping a receiver whose sender still produces
- `Arc<Mutex<_>>` used as a default design habit rather than a considered choice
- `Atomic*::update` / `try_update` over a hand-rolled `compare_exchange` retry loop. `Requires Rust >= 1.95`

The prescribed direction for this rule is **ownership transfer and message passing** over pervasive
shared state. Use it when writing the `fix` field.

### 3. RUST-PERF-001 — No Obvious Performance Footguns
**Severity**: MEDIUM

Restraint applies to this rule more than any other: do not micro-optimize blindly, report only clear
and likely-relevant footguns, and prefer evidence-based comments. It is the lowest-severity rule
here and the easiest to flood a review with.

- **N+1 queries or repeated expensive work in a hot path** — a per-row database lookup inside a loop is the highest-value finding in this rule
- **Data structures that do not fit the access pattern**, e.g. `Vec::contains` in a loop where a `HashSet` belongs
- **Work repeated unnecessarily** — a value recomputed per iteration that could be hoisted
- Allocations excessive without reason
- **Expensive formatting or logging evaluated eagerly** in a hot path: `debug!("{}", expensive())` pays for the argument even when the level is disabled
- Unbounded collections that can grow without limit
- O(n²) where O(n) is feasible
- `.collect::<Vec<_>>()` immediately consumed by another loop instead of chaining iterators. `Enforcement: clippy needless_collect (deny)`
- `Box::new([0; N])` for a large buffer instead of `vec![0; n]`
- `map.get(&key.to_string())` or a `.clone()` at a lookup site — a `HashMap<String, V>` is queryable with `&str`
- Oversized enum variants that should be boxed, and double indirection `Box<Vec<T>>` / `Box<String>`. `Enforcement: clippy rc_buffer (deny)` for the `Arc`/`Rc` shapes only
  why: the lint covers `Arc<String>`/`Rc<String>`, so post only the `Box` shapes and
       `large_enum_variant`.
- Repeated `+` or `format!` reallocation in a hot path where `String::with_capacity(n)` plus `push_str` avoids it
  why: review-only. Only the `push_str`-chain-instead-of-`format!` shape is clippy-denied.
- **Large stack frames** in tasks with small stacks. `Enforcement: clippy large_stack_arrays (deny)` covers large stack *arrays*; frames are review-only
- Hand-rolled shift/mask bit arithmetic where a std method exists. `Requires Rust >= 1.97`
  why: `bit_width`, `isolate_highest_one`, `isolate_lowest_one`, `highest_one` and `lowest_one`
       cover it, and the hand-rolled form usually mishandles the zero case.
- `to_string()`/`format!` per iteration in a hot integer-formatting loop, where `core::fmt::NumBuffer<T>` + `format_into` reuses one stack buffer. `Requires Rust >= 1.98`
- `core::hint::cold_path()` used as anything more than a hint — it must never be load-bearing for correctness. `Requires Rust >= 1.95`

Skip in this repo, all clippy-denied: gratuitous clones, `&str.to_string()`, `with_capacity(0)`,
`String::from("lit")`, `push_str` chains, large stack arrays (`redundant_clone`, `str_to_string`,
`manual_string_new`, `format_push_string`, `large_stack_arrays`). Do not spend a finding slot on
them. Note this does **not** cover "allocations excessive without reason" above, which is broader
than `redundant_clone` and stays postable.

### 4. RUST-NO-004 — No Async Blocking Footguns
**Severity**: CRITICAL

RUST-ASYNC-001 restated as prohibitions, plus one criterion of its own:

- No blocking file, network, database, sleep, or CPU-heavy work directly inside an async task without appropriate handling
- No `.await` while holding a broad or long-lived lock unless explicitly justified
- **Unbounded fan-out of tasks**: `for item in items { tokio::spawn(...) }` over caller-controlled input
  why: it spawns without limit. Require a bounded primitive — a semaphore, a worker pool, or
       `buffer_unordered` with a cap.

### 5. RUST-NO-005 — No Unjustified Shared Mutability
**Severity**: HIGH

- No `Arc<Mutex<_>>` as a default convenience pattern. The judgement is about habit, not about whether any single occurrence carries a comment
- No pervasive **interior mutability** where plain ownership would work — this covers `Cell`, `RefCell`, `OnceCell`, `Mutex` and atomics, not only `Arc<Mutex<T>>` and `Arc<RwLock<T>>`
- **No overly broad lock-protected state blobs** — the `Arc<Mutex<AppState>>` holding everything, or an `Arc<Mutex<HashMap<..>>>` growing into a hidden subsystem
