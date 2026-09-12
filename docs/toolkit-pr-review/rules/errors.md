# Rule module: Error Handling and Panic Safety

This file is a rule module. Exactly one agent reads it, and that agent reads no other
module. See `docs/toolkit-pr-review/agents/subject.md` for how that agent works,
`docs/toolkit-pr-review/review-conventions.md` for severity and marker
conventions, and `docs/toolkit-pr-review/comment-style.md` for how a finding is worded.

## Scope of this module

Apply every rule in this module to every changed file in the PR.

## Check IDs to Apply

Apply **only** these specific check IDs. Each rule's `**Severity**` is the value to put in the
finding; do not infer it from the example in the Output Contract.

### 1. RUST-ERR-001 — Error Handling Is Explicit and Useful
**Severity**: CRITICAL

- [HIGH] Fallible operations return `Result` where failure is expected, not `Option`, a sentinel value, or a bare `bool`
- [HIGH] Error context is preserved. Flag `map_err(|_| ...)` that discards the source error
- Errors are not swallowed **or silently downgraded** — a CRITICAL condition turned into a logged warning, or a typed error collapsed into `Option`, is a finding
- [MEDIUM] Error messages are actionable
- [HIGH] Domain errors are distinguishable where that matters. Reusing one generic variant for distinct domain conditions breaks pattern matching by callers
- [HIGH] The code does not rely on logs alone instead of returning errors
- [MEDIUM] Generic error wrapping that hides the root cause **without reason**. Deliberate, justified wrapping is fine; the finding is unexplained loss of the cause
- [MEDIUM] Ad hoc stringification (`.map_err(|e| MyError::Other(e.to_string()))`) where propagation with context belongs. This is the most common shape of this rule in practice
- `From` used for a conversion that can fail — a `From` impl that panics or silently coerces on bad input is a correctness bug. Use `TryFrom`
- [LOW] A stack of `if cond { return Err(..) }` guards where `bool::ok_or` reads better, as in `user.is_active().ok_or(Error::Inactive)?`. `Requires Rust >= 1.98`
  why: the receiver is the **`bool`** condition that must hold. This is the method on `bool`,
       stabilised in 1.98, not `Option::ok_or`, which has existed since Rust 1.0 — never apply
       the gate to an `Option` receiver.
- [MEDIUM] A large error payload returned unboxed, or `Result<_, ()>` from an async signature. The rule holds on any toolchain; only the lint coverage on `async fn` is new. `Requires Clippy >= 1.98`

### 2. RUST-PANIC-001 — Panic Safety
**Severity**: HIGH

- No `unwrap()`, `expect()`, or `panic!()` in production paths without strong justification. A `panic!("tenant {id} missing")` in a request handler is a finding even though it carries a message
- `unreachable!()` only where the invariant is truly guaranteed. No lint covers this, so review is the only line of defence
- Assertions (`assert!`, `assert_eq!`, `debug_assert!`) are not used as ordinary runtime validation in production code
- Panics are reserved for impossible states, **tests, examples**, or process-fatal initialization where justified. Do not flag a panic in test or example code
- In **library** code a panic is a much stronger smell than in service code. Apply the stricter bar there
- Flag panic-prone code specifically in request paths, workers, retries, background tasks and data pipelines — call-site context escalates severity
- `v[i]` or `&s[a..b]` where the index **derives from external input**, instead of `.get()` or a pattern match. A provably-safe index is not a finding
- Byte-range slicing a `str` (`&s[..8]`) with externally derived bounds, which panics inside a multi-byte UTF-8 sequence. Use `s.get(..8)` or `chars().take(n)`
- An `is_empty()`/`len()` check decoupled from a later `v[0]`/`v[i]` access, where matching on `v.as_slice()` (`[]`, `[one]`, `[first, rest @ ..]`) lets the compiler prove the access
- `push`/`insert` followed by `last_mut().unwrap()`/`.expect()`. `Requires Rust >= 1.95`
  why: `Vec::push_mut`, `Vec::insert_mut`, `VecDeque::push_{front,back}_mut` and
       `LinkedList::push_{front,back}_mut` hand back `&mut T` with no unwrap.
- An unbounded integer or char range loop (`for i in 250u8..`) that wraps, panics, or never terminates. The bug is real on any toolchain; only the lint is new. `Requires Clippy >= 1.98`
- An `unwrap`/`expect` exception that does not state why the value is guaranteed `Some`/`Ok`. `Enforcement: clippy unwrap_used, expect_used (deny)` for the bare call, so the missing justification is the part you post
  why: a `const` initializer is the only broadly defensible case. In service code a startup
       invariant qualifies only with `#[expect(clippy::expect_used, reason = "...")]` plus a
       very clear message.
- The fix for an `unwrap()` whose value has a sensible default is `unwrap_or`/`unwrap_or_else`/`unwrap_or_default`. `Enforcement: clippy unwrap_used (deny)`

### 3. RUST-NO-001 — No Placeholder Production Logic
**Severity**: CRITICAL

- No `todo!()`, `unimplemented!()`, **stub returns, fake success, or empty implementations** in production paths. `Ok(())` and `Ok(vec![])` standing in for unwritten logic are the shapes that actually ship
- No placeholder branches that silently discard work
- No fake adapters presented as complete behavior **unless clearly test-only**

Scope matters here: the rule says *production paths*, and the fake-adapter criterion has an explicit
test-only exemption. A `todo!()` inside `#[cfg(test)]` code or a deliberate test fixture is not a
finding.

### 4. RUST-NO-002 — No Silent Failure
**Severity**: CRITICAL

- No ignored `Result` for a fallible operation without justification
- **`let _ = ...` on a meaningful failure**, unless explicitly intentional and documented
  why: `let _ = tx.send(msg);` is the canonical silent-failure idiom in Rust, and
       `let_underscore_must_use` does not catch it when the return type is not `#[must_use]`.
- [HIGH] No empty error handlers (`_ => { }`)
- **No failure path that only logs and continues** where correctness requires propagation or a state change
- [HIGH] No `iter.by_ref().peekable().peek()` — it silently consumes and discards an item. Bind the `Peekable` or call `.next()`. `Requires Clippy >= 1.98`
- [HIGH] No `mem::forget` on a type with a `Drop` impl; almost always a leak bug rather than an intentional leak

### 5. RUST-NO-003 — No Panic-Driven Control Flow
**Severity**: HIGH

- No `unwrap()` / `expect()` used as ordinary control flow. This still applies to a *justified* unwrap: the question is whether panicking is the control-flow mechanism, not whether the call is annotated
- No panic used instead of validation or typed error handling
- **A "this can never fail" assumption whose invariant is not obvious and local**
  why: an invariant asserted three call frames away is not local, and that is the test that
       lets you reject an otherwise justified `unwrap`.
