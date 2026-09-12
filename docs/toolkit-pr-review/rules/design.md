# Rule module: Design, Types and Architecture

This file is a rule module. Exactly one agent reads it, and that agent reads no other
module. See `docs/toolkit-pr-review/agents/subject.md` for how that agent works,
`docs/toolkit-pr-review/review-conventions.md` for severity and marker
conventions, and `docs/toolkit-pr-review/comment-style.md` for how a finding is worded.

## Scope of this module

Apply every rule in this module to every changed file in the PR.

The PR-level architecture pass is **not** here. It runs once over the whole PR in a separate
agent (`docs/toolkit-pr-review/agents/architecture.md`), because it needs a view of the
entire diff, and it belongs to the architecture agent. Do not attempt it here.

## Check IDs to Apply

Apply **only** these specific check IDs. Each rule's `**Severity**` is the value to put in the
finding; do not infer it from the example in the Output Contract.

### 1. RUST-API-001 — Idiomatic Public API Design
**Severity**: HIGH

- Names are clear and conventional. This is about semantic clarity, not casing — rustc already warns on casing
- Types express meaning better than raw `bool`, `String`, or loosely structured maps
- **Arguments are hard to misuse** — two same-typed positional parameters that can be swapped silently, for example
- **A builder is missing where construction is complex**
  why: the shapes that qualify are a `new()` with many optional parameters, or a telescoping
       `with_*` chain over a struct with several `Option` fields.
- A builder that exists but is non-standard, or an ad hoc factory method
- **Trait boundaries are purposeful and not overly broad**
- Public APIs expose the minimum necessary surface; visibility is minimal (`pub` only where needed)
- **Return types are ergonomic and predictable**
- Implementation details in public signatures
- Owned-string parameters forcing callers to pre-convert, where `impl Into<String>` belongs
- Validating constructors that panic or silently clamp instead of returning `Result`
- `Deref`/`DerefMut` faking inheritance on a wrapper type. Prefer explicit delegation or a trait
  why: it leaks the inner type's full API and blurs ownership.
- Third-party types in public signatures (`reqwest::Error`, `sqlx::Row`, `hyper::Uri`)
  why: wrapping them lets the dependency be swapped without a breaking change.
- Public config and value types missing either `Default` or an inherent `new()`
- Library value types with `pub` fields where a validated constructor belongs
  why: `pub` fields make every invariant a caller responsibility and are a semver commitment.
- A hand-rolled `match` on a kind/tag field that dispatches behavior, where a trait object or an `Fn` strategy parameter belongs
- A local extension trait or free helper where the orphan rule calls for a newtype
- New public types with no `Debug` impl, and new `pub` items in a library crate with no `///` docs. `Enforcement: clippy missing_errors_doc, missing_panics_doc (deny)` for the doc sections only
  why: the lint covers the `# Errors` and `# Panics` sections. The `Debug` impl and the item
       doc itself are review-only, so those are the parts you post.
- [LOW] Boolean parameters: **exactly 3** on one function. Suggest an enum or a builder, never an options struct
  why: 4 or more is already build-rejected by `fn_params_excessive_bools`, reached through the
       denied `pedantic` group, so exactly 3 is the only postable band. An options struct with
       3 bool fields does not compile either: `clippy.toml` sets `max-struct-bools = 2` and
       `struct_excessive_bools` is denied.

### 2. RUST-TYPE-001 — Type Safety and Invariants
**Severity**: HIGH

- Important invariants are enforced by types where practical; invalid states unrepresentable when reasonable
- **`Option` and `Result` used intentionally, not as vague escape hatches**
- Newtypes where they improve safety or readability
- **Distinct concepts mixed through aliases of the same primitive** — `type UserId = u64; type OrderId = u64;` lets the two be swapped silently
- **Lifetimes and the ownership model used to prevent misuse, not bypassed with clones or shared mutability**
- **Prefer compile-time guarantees over comments**
- **APIs that rely on caller discipline where the type system could help**
- Wildcard `_ =>` catch-all on a crate-owned enum where an exhaustive match would force new variants to be handled. `Requires Rust >= 1.95`
  why: `if let` guards do **not** participate in exhaustiveness checking, so a `match` that
       looks total may not be, and an `if let`-guarded arm is not a justification for dropping
       the wildcard.
- Public enums/structs expected to grow that are missing `#[non_exhaustive]`
- Easily-dropped-by-accident results (builders, "must apply" configs, guards) missing `#[must_use]`
- Bare `as` for numeric conversion. `Enforcement: clippy cast_possible_truncation, cast_possible_wrap, cast_precision_loss, cast_sign_loss, cast_lossless, ptr_as_ptr (deny)` — **not postable**, keep it in mind only when judging a related design choice
  why: widening wants `u64::from(x)`, narrowing wants `TryFrom` with a real error path, and
       pointer casts want `.cast::<T>()`. `cast_lossless` and `ptr_as_ptr` arrive through the
       denied `pedantic` group, so the build already rejects every shape here.
- Comparison traits all-manual or all-derived over the same field set: `a == b` must hold exactly when `cmp` returns `Equal`. `Requires Rust >= 1.98` for the derive fast path that exposes it
- A type with a manual `PartialEq` used in a constant pattern. `Requires Rust >= 1.98`
- `#[repr(transparent)]` over a `#[non_exhaustive]`, `repr(C)`, or private-field type; keep it only for real ABI or `transmute` needs. `Requires Rust >= 1.98`
- `parse()` followed by `NonZero::new(..).ok_or(..)` where `NonZero::<u32>::from_str_radix(s, 10)?` makes zero a parse error. `Requires Rust >= 1.98`
- Manual `PartialEq`/`Hash`/`Ord` impls that do not destructure `Self { .. }` **and name every ignored field**. `Enforcement: clippy missing_fields_in_debug (deny)` for `Debug` only
  why: without the destructure a field added later is silently ignored instead of breaking the
       build. The lint covers `Debug`, so the other three impls are yours.
- A bare `_` or `..` in a destructuring pattern where a named ignore (`has_fuel: _`) would show which field was deliberately skipped
- `..Default::default()` in a crate-owned struct literal: a field added later is silently defaulted at every construction site

### 3. RUST-OWN-001 — Ownership and Borrowing
**Severity**: MEDIUM

- Unnecessary cloning. Flag **defensive cloning without evidence** — the evidence bar matters here, do not flag a clone you cannot show is avoidable
- References preferred when ownership transfer is not needed
- **`Arc`, `Rc`, `Mutex`, `RwLock` and interior mutability used only when justified**
- **Large values copied or moved unnecessarily**
- **Borrowing structure keeps APIs ergonomic and efficient** — flag ownership patterns that make an API awkward or expensive
- **Unnecessary heap allocation or conversion churn**
- Owned-type parameters where a borrowed one would do: `&String` for `&str`, `&Vec<T>` for `&[T]`, `&Box<T>` for `&T`
- Cloning to move a field out of `&mut self` or an enum variant instead of `mem::take`/`mem::replace`
- A fallible function that takes ownership of an argument but does not return it inside the error variant, forcing the caller to re-clone before retrying
- A single lifetime parameter bounding both an input argument and a stored or returned reference — it over-constrains callers; use separate lifetimes
- Manual acquire/release pairs where an RAII guard (`Drop`) would be safer
- A clone, `RefCell`, or `Arc` introduced only to borrow two fields of the same struct — decompose the struct into independently borrowable parts
- A `move` closure or spawned task capturing `self` or a whole context instead of rebinding the narrowest value in a scope block, and a refcount bump written `x.clone()` instead of `Arc::clone(&x)`
- A `let mut` binding that stays mutable long after setup, where `let x = { let mut x = ..; x.sort(); x };` confines the mutability

### 4. RUST-DATA-001 — Serialization and Data Contracts Are Stable
**Severity**: HIGH

- `serde` attributes intentional and correct
- **Field renames, defaults, enum formats and optionality safe for the intended contract** — all four mechanisms, not just "breaking changes"
- Backward and forward compatibility considered where relevant
- **Deserialization failures remain diagnosable**
- **Time, UUID and numeric formats handled consistently**
- Accidental wire-format changes
- **Fragile enum or string handling**
- **Implicit defaults that can hide contract bugs**
- `n != 0` when decoding a strictly-0/1 wire or storage flag. `Requires Rust >= 1.95`
  why: `bool::try_from(n).map_err(...)` surfaces out-of-contract bytes as a parse error instead
       of reading them as `true`.
- `f32`/`f64` `algebraic_*` operations on a value that is compared, sorted, hashed, serialized, persisted, asserted on, or drives billing, quota or alert thresholds. `Requires Rust >= 1.98`
  why: they permit reassociation, so results vary across call sites, optimization levels and
       targets.

### 5. RUST-OBS-001 — Logging, Tracing and Metrics
**Severity**: MEDIUM

- Important failures logged at the right boundary
- Logs carry enough context to debug a production issue
- **Sensitive data is not emitted** — this is broader than request bodies: a bare `tracing::info!(token = %t)` is a finding
- **Request, task and job identifiers propagated where relevant**
- **Critical operational paths with no metrics or tracing, in a long-running service**
  why: long-running is the applicability gate. A background reconciler with no metrics is a
       finding even if it is not hot.
- **Duplicate logging of the same error at several layers**, unless intentional
- **Logs with no identifiers or context**
- **Missing observability in background workers, retries, queue processing and external calls**
- Unstructured log calls where structured `tracing` belongs
- Authentication attempts or authorization denials not logged through structured `tracing`
  why: authentication needs success **and** failure with the source IP; a denial needs identity,
       requested resource and reason. Never log request or response bodies that may carry
       credentials or PII.

### 6. RUST-OBS-002 — No Debug Artifacts in Production Output
**Severity**: MEDIUM

- No `println!()`, `eprintln!()`, or direct writes to stdout/stderr in library or service code; diagnostics belong in `tracing::{trace,debug,info,warn,error}`
- `Enforcement: clippy dbg_macro, use_debug (deny)` covers `dbg!` and `{:?}` in output, so only the print macros are yours
- A CLI binary writing its intended program output to stdout is not a finding; a service or library doing it is

### 7. RUST-MOD-001 — Gear Boundaries and Code Organization
**Severity**: HIGH

- Responsibilities separated clearly
- **Business logic not tangled with transport, persistence or framework glue**
- **Helpers not used to hide poor structure**
- **Gears are cohesive**
- Visibility and dependency direction intentional
- **The PR does not introduce avoidable architectural drift**
- **"God gears"** — one gear accreting unrelated responsibilities
- **Handlers or controllers doing domain work directly instead of delegating**
- **Infrastructure details leaking into domain logic without need**
- New platform-gated code using the `cfg-if` crate where std `cfg_select!` belongs. Do not ask for existing `cfg_if!` usages to be migrated. `Requires Rust >= 1.95`
- **Several distinct responsibilities fused into one construct**, and it is **postable**
  why: for example a `select!` arm holding the watch branch, the poll branch and the elapse
       classification in one inline async block, or a match arm that decides, performs and
       reports in one place. `cognitive_complexity` counts branches, not concerns, so a block
       can be under the threshold and still be doing three separable jobs.
- Raw function complexity and length. `Enforcement: clippy cognitive_complexity, too_many_lines (deny)` — the numeric case is **not postable**, the structural case above is yours
  why: the lint fires at the `clippy.toml` thresholds, cognitive complexity 20 and 200 lines.
- Source-level blanket lint escalation belongs to RUST-LINT-001, not here

### 8. RUST-LINT-001 — In-Source Lint Suppression Hygiene
**Severity**: MEDIUM

- An `#[allow(...)]` or `#[expect(...)]` with no `reason = "..."`
- `#[allow]` where `#[expect]` would self-remove once the suppression stops being needed
- **Suppressions wider than necessary** — a module-level `#![allow(clippy::some_lint)]` is neither per-item nor crate-level, and still a finding
- A crate-level group `allow` (`clippy::all`, `clippy::pedantic`, `clippy::nursery`) added in source
- `#![deny(warnings)]` or equivalent blanket escalation in source. Deny specific lints by name, or gate it in the build. `Requires Rust >= 1.97` for the `build.warnings` form
  why: a blanket escalation turns any future lint, including new compiler lints, into a hard
       build break for downstream consumers. Prefer `build.warnings = "deny"` or
       `CARGO_BUILD_WARNINGS=deny` over `RUSTFLAGS="-D warnings"`, which applies beyond local
       packages and busts the build cache.
- `unused_async_trait_impl` silenced crate-wide instead of per impl where a foreign trait mandates `async fn`
- `Enforcement: clippy ignore_without_reason (deny)` covers `#[ignore]` on tests only, so do not post that shape. The general `reason` requirement is review-only

### 9. RUST-NO-007 — No Contract Drift by Accident
**Severity**: HIGH

- No accidental API breakage
- No accidental serde, wire or **schema** changes
- **No accidental visibility expansion**
- **No accidental behavior change hidden inside a refactoring** — the archetypal contract-drift defect and the reason this rule exists
- New blanket trait impls (`impl<T: Bound> Trait for T`) in a public API without deliberate justification
  why: a semver and coherence hazard for downstream crates. The fix is to seal the trait behind
       a private supertrait so only this crate can implement it, or to write per-type impls.
- **`#[non_exhaustive]` added to an already-public type**, which is itself a breaking change
  why: every out-of-crate struct literal stops compiling. RUST-TYPE-001 asks for it on types
       *expected to grow*, meaning at introduction; retrofitting it onto a shipped type is
       contract drift. A tell is the same PR rewriting construction sites in its own tests.
- **A type's declared capability disagreeing with its behavior**
  why: if `features()` reports a capability as absent, the corresponding method must fail in
       the way callers match on; if it reports it present, the method must not fail as
       unsupported. A mismatch inside one impl is a broken contract even when each half reads
       correctly on its own.
- Public API changes with no accompanying documentation update
