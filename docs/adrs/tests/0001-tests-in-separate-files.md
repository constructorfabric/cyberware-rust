---
status: accepted
date: 2026-04-15
decision-makers: Constructor Fabric Steering Committee
---
<!-- Created: 2026-04-15 by Constructor Tech -->

# Enforce test code in separate files vian architecture lint DE1101

**ID**: `cpt-cf-adr-tests-in-separate-files`

## Table of Contents

<!-- toc -->
<!-- /toc -->

## Context and Problem Statement

Rust modules in the gears-rust monorepo contain inline `#[cfg(test)] mod tests { ... }` blocks that mix production and test code in the same file. As modules grow, these inline test blocks cause large files (500+ lines of tests alongside production code), inaccurate LOC metrics, noisy PR diffs, and harder navigation for both humans and LLMs. Should the project adopt a stricter convention than the Rust Book default?

## Decision Drivers

* Production files should stay focused on production logic.
* Test code should be easy to filter out from metrics, search, and code review.
* The convention must be enforceable automatically (not just a style guide).
* Migration from inline to separate files must be incremental — not a big-bang rewrite.
* Small test blocks (under a configurable threshold) should not require separation.
* Once tests are moved to a separate file, they must not be added back inline.

## Considered Options

* Keep inline tests (Rust Book default)
* Separate test files with an architecture lint
* Integration tests only (`tests/` directory)

## Decision Outcome

Chosen option: "Separate test files with an architecture lint", because it is the only option that provides automatic enforcement, supports incremental migration, and preserves the ability to test `pub(crate)` internals (via `#[path]` module reference which compiles as part of the crate).

### Consequences

* All new gears must follow the `{stem}_tests.rs` companion file convention from day one.
* Existing modules are migrated incrementally by removing entries from `excluded_paths` in the lint configuration in `Gears.toml`.
* The `extract_tests.py` migration script must be maintained alongside the lint.
* Developers unfamiliar with the convention will see a clear lint error message explaining what to do.
* CI pipeline adds ~15 seconds for the `cargo gears lint` check.

### Confirmation

Compliance is confirmed automatically: CI runs `cargo gears lint` which includes lint DE1101. Any inline test code exceeding the threshold or violating the companion file guard produces a build error. The `excluded_paths` list in the lint configuration in `Gears.toml` tracks modules not yet migrated.

## Pros and Cons of the Options

### Keep inline tests (Rust Book default)

Follow the standard Rust convention: `#[cfg(test)] mod tests { ... }` inline in the same file.

* Good, because it is idiomatic Rust — no surprise for new contributors.
* Good, because test code is colocated with production code — easy to find.
* Good, because no tooling required.
* Bad, because files grow large (some are 2000+ lines with inline tests).
* Bad, because LOC metrics include test code.
* Bad, because PRs mix production and test changes in the same diff.
* Bad, because no automatic enforcement — relies on code review.

### Separate test files with an architecture lint

Enforce `*_tests.rs` companion files via a custom architecture lint (DE1101). The lint denies inline test blocks exceeding `max_inline_test_lines` (default: 100), denies any inline test when a companion file exists, and validates `#[path]` attributes.

* Good, because production files contain only production code.
* Good, because test files are instantly identifiable by naming convention (`*_tests.rs`).
* Good, because LOC tools automatically exclude test files.
* Good, because PR diffs are cleaner — production changes in one file, test changes in another.
* Good, because small test blocks (< 100 lines) are allowed inline — no churn for trivial tests.
* Good, because companion file guard prevents test code from being split across two files.
* Good, because incremental migration via `excluded_paths` — modules are migrated one by one.
* Good, because smaller files reduce LLM context window usage — agents process production logic without loading test code.
* Bad, because it deviates from Rust Book convention — may surprise Rust developers.
* Bad, because it requires tooling (architecture lint + migration script).
* Bad, because navigation between production and test files requires one extra step.

### Integration tests only (`tests/` directory)

Move all tests to the `tests/` directory as integration tests.

* Good, because clean separation — test binaries are completely separate.
* Good, because it is a standard Rust pattern for integration tests.
* Bad, because integration tests cannot access `pub(crate)` items — requires making internals `pub`.
* Bad, because slower compilation — each test file is a separate crate.
* Bad, because it loses the benefit of unit tests that can access private module state.

## Configuration

```toml
# the lint configuration in `Gears.toml`
[de1101_tests_in_separate_files]
max_inline_test_lines = 100
excluded_paths = [
    "libs/toolkit",
    "gears/mini-chat",
    # ... gears not yet migrated
]
```

### File naming convention

| Production file | Test file |
|---|---|
| `handler.rs` | `handler_tests.rs` |
| `service.rs` | `service_tests.rs` |
| `models.rs` | `models_tests.rs` |

The production file references its companion via:
```rust
#[cfg(test)]
#[path = "handler_tests.rs"]
mod handler_tests;
```

### Threshold behavior

- Inline test blocks **under** `max_inline_test_lines` (default 100) are allowed without triggering the lint.
- Once a companion `{stem}_tests.rs` file exists, **any** inline test code is denied regardless of size.
- Bare `#[test]` functions outside a `#[cfg(test)]` module are always denied.

### Migration path

1. Run `extract_tests.py <directory>` to automatically split inline tests into companion files.
2. Remove the module from `excluded_paths` in the lint configuration in `Gears.toml`.
3. CI enforces the convention going forward.

## References

- [Rust Book ch11-03: Test Organization](https://doc.rust-lang.org/book/ch11-03-test-organization.html)
- Architecture lint DE1101 (via `cargo gears lint`)
- [Unit & Integration Testing Guide](../../toolkit_unified_system/12_unit_testing.md)
