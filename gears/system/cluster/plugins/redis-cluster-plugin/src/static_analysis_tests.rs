//! TESTING.md §6's mechanical source checks: the commands this plugin must
//! never issue, asserted over its own source rather than left to review
//! vigilance.
//!
//! Only one of §6's two rules needs a test at all, and knowing which is the
//! point of this file:
//!
//! - **The `B*` blocking family is a compile-time impossibility**, not a
//!   convention. DESIGN.md §3.1 names `fred`'s interface features individually and leaves
//!   `i-lists` out, so `BLPOP`/`BRPOP`/`BLMOVE`/`BLMPOP` are not in the trait
//!   surface this crate can see: reaching for one is a type error, which is a
//!   stronger guarantee than any scan. Scanning for them here would be actively
//!   counterproductive — `lock/mod.rs` and `lock/waiters.rs` both *name* `BLPOP`
//!   in prose, explaining why the release-waiter registry exists instead of it,
//!   and a text scan would turn that explanation into a failure.
//! - **`KEYS`, `FLUSHALL`, and `FLUSHDB` are all reachable**, so they are what
//!   this file checks. `KEYS` is in `i-keys` alongside the `GET`/`SET`/`PEXPIRE`
//!   family the cache is built on, and `flushall`/`flushdb` sit on `ClientLike`
//!   itself — ungated by any feature, so no feature list can put them out of
//!   reach. `KEYS` on a shared production Redis is an outage rather than a slow
//!   query (DESIGN.md §4.4): it is O(N) over the whole keyspace and blocks the
//!   single-threaded server for the duration, which is why `cache/scan.rs`
//!   iterates with a `SCAN` cursor. `FLUSHALL`/`FLUSHDB` would delete other
//!   tenants' data, not merely this plugin's.
//!
//! ## Why the check is on the call form, not the command name
//!
//! Matching the bare word `KEYS` is unusable here: `preflight.rs`'s
//! `REQUIRED_KEYSPACE_FLAGS`, `observability.rs`'s
//! `KEYSPACE_NOTIFICATIONS_SET`, and every Lua script's `KEYS[1]` all contain
//! it, and the last of those is the *correct* use — `KEYS` is Lua's argument
//! global, not a command, inside a script body. So this scans for the Rust
//! method call that would actually issue the command (`.keys(`), and
//! `scripts_tests.rs` covers the Lua half separately by looking for the
//! `redis.call` form.
//!
//! Test sources are excluded from the scan: the forbidden strings necessarily
//! appear in this file, and §6's rule is about the shipped plugin.

use std::path::{Path, PathBuf};

/// Method-call fragments that would issue a command this plugin must never
/// issue, paired with why each is disqualifying.
///
/// The trailing `(` is load-bearing: it is what distinguishes a call from a
/// mention in a doc comment, and it is why `keys` — a common enough word — can
/// be matched at all without false positives on prose or on identifiers like
/// `MultipleKeys`.
const FORBIDDEN_CALLS: &[(&str, &str)] = &[
    (
        ".keys(",
        "KEYS is O(N) over the whole keyspace and blocks the single-threaded server for the \
         duration, so on a shared production Redis it is an outage rather than a slow query. Use \
         the SCAN cursor loop in cache/scan.rs (DESIGN.md sec 4.4)",
    ),
    (
        ".flushall(",
        "FLUSHALL deletes every key on the server, including those of unrelated tenants sharing \
         it. This plugin owns only its own key prefix and must never reach past it",
    ),
    (
        ".flushdb(",
        "FLUSHDB deletes every key in the logical database, including those of unrelated tenants \
         sharing it. This plugin owns only its own key prefix and must never reach past it",
    ),
];

/// Every non-test `.rs` file under `src/`, as (path, contents).
///
/// Walked rather than listed so a module added later is covered without anyone
/// remembering to add it here — a check that silently stops covering new code is
/// worse than no check, because it still reads as a green guarantee.
fn plugin_sources() -> Vec<(PathBuf, String)> {
    fn walk(dir: &Path, found: &mut Vec<(PathBuf, String)>) {
        let entries = std::fs::read_dir(dir).expect("the crate's src/ directory is readable");
        for entry in entries {
            let path = entry.expect("a src/ directory entry is readable").path();
            if path.is_dir() {
                walk(&path, found);
                continue;
            }
            let is_rust = path.extension().is_some_and(|ext| ext == "rs");
            let name = path
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .unwrap_or_default();
            // `*_tests.rs` and `test_support.rs` are `#[cfg(test)]`-only and are
            // not part of the shipped plugin; this file's own forbidden-string
            // table would otherwise fail the check it defines.
            let is_test = name.ends_with("_tests.rs") || name == "test_support.rs";
            if is_rust && !is_test {
                let contents =
                    std::fs::read_to_string(&path).expect("a src/ Rust file is valid UTF-8");
                found.push((path, contents));
            }
        }
    }

    let mut found = Vec::new();
    walk(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut found,
    );
    assert!(
        !found.is_empty(),
        "the source walk found nothing, so this check is passing vacuously"
    );
    found
}

#[test]
fn no_source_file_issues_a_keyspace_wide_command() {
    for (path, contents) in plugin_sources() {
        for (call, why) in FORBIDDEN_CALLS {
            assert!(
                !contents.contains(call),
                "{} issues `{call}`, which TESTING.md sec 6 forbids anywhere in src/: {why}",
                path.display()
            );
        }
    }
}

/// Marks a `warn!`/`error!` that is deliberately not an ADR-004 catalog event,
/// placed on the line above the macro with the reason after it.
///
/// The escape hatch exists because not every warning is an operator's business.
/// The ADR-006 `Drop` guards are the whole of the current set: they fire on a
/// *programming error*, in the release build of the same arm that panics in
/// debug, and cataloguing them would put a developer diagnostic in the event
/// table an operator alerts on.
const CATALOG_EXEMPT: &str = "not-a-catalogued-event:";

/// Whether `line` opens a `warn!`/`error!` macro call in code rather than prose.
///
/// The line is truncated at the first `//` so a doc comment naming the macro —
/// this file's own module docs, for one — is not read as a call site. That
/// truncation is why the check is on the macro-open line rather than on the
/// whole invocation: a `//` inside a message string would cut the scan short,
/// and the message never sits on the opening line.
fn opens_a_warn_or_error(line: &str) -> bool {
    let code = line.split("//").next().unwrap_or_default();
    code.contains("warn!(") || code.contains("error!(")
}

/// Whether the comment-and-attribute run immediately above `index` carries
/// [`CATALOG_EXEMPT`].
///
/// The whole run rather than the single line above it, because the reason for an
/// exemption is worth a sentence and a sentence wraps — and because a
/// `#[cfg(...)]` frequently sits between the comment and the macro.
fn is_exempted(lines: &[&str], index: usize) -> bool {
    let mut cursor = index;
    while let Some(before) = cursor.checked_sub(1) {
        let line = lines[before].trim_start();
        if !line.starts_with("//") && !line.starts_with("#[") {
            return false;
        }
        if line.contains(CATALOG_EXEMPT) {
            return true;
        }
        cursor = before;
    }
    false
}

#[test]
fn every_warn_and_error_carries_a_catalogued_event_name() {
    // DESIGN.md §9's contract, held mechanically. A WARN without a `name:` field
    // cannot be matched structurally by a collector, so the condition it reports
    // is unalertable however severe it is — which is how
    // `cluster.provider.subscriber_lost`, the line announcing that the
    // subscriber is permanently gone, went uncatalogued.
    //
    // `name:` is the macro's first argument by convention
    // (`observability.rs`'s "carry its name twice"), so it is on the opening
    // line or the one after it.
    for (path, contents) in plugin_sources() {
        let lines: Vec<&str> = contents.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            if !opens_a_warn_or_error(line) {
                continue;
            }
            let next = lines.get(index + 1).copied().unwrap_or_default();
            if line.contains("name:") || next.contains("name:") {
                continue;
            }
            assert!(
                is_exempted(&lines, index),
                "{}:{} emits a WARN or ERROR with no `name:` field. A collector filters \
                 structurally on `name`, so an event without one is unalertable (DESIGN.md sec 9). \
                 Add a constant to `observability::logs` and a row to DESIGN.md sec 9's table, or \
                 mark the site `{CATALOG_EXEMPT} <reason>` on the line above if it is a developer \
                 diagnostic rather than an operator's business",
                path.display(),
                index + 1
            );
        }
    }
}

#[test]
fn the_event_name_scan_is_capable_of_failing() {
    // The same guard `the_scan_is_capable_of_failing` puts on the forbidden-call
    // matcher: a scan that cannot fire is indistinguishable from one whose
    // pattern stopped matching anything.
    assert!(
        opens_a_warn_or_error("        tracing::warn!("),
        "the matcher must recognize a real macro-call site"
    );
    assert!(
        opens_a_warn_or_error("            DropDiagnosis::DuringPanic => warn!("),
        "including one opened part-way through a match arm"
    );
    assert!(
        !opens_a_warn_or_error("/// so a doc comment writing warn!( is not a call site"),
        "prose naming the macro must not match"
    );

    // The exemption is found across a wrapped comment and an intervening
    // attribute, which is the shape both ADR-006 `Drop` guards actually have,
    // and is not found when the run above the site says nothing.
    let exempted = [
        "// not-a-catalogued-event: a developer diagnostic,",
        "// not an operator's business.",
        "#[cfg(not(debug_assertions))]",
        "warn!(",
    ];
    assert!(is_exempted(&exempted, 3));
    let unmarked = ["// just an ordinary comment", "warn!("];
    assert!(!is_exempted(&unmarked, 1));
}

#[test]
fn the_scan_is_capable_of_failing() {
    // A source-scanning check that cannot fail is indistinguishable from one
    // that passes because the pattern never matches anything — a `.keys(`
    // renamed upstream, say, or a walk that quietly stopped finding files. This
    // pins that the matcher itself works on text known to contain a violation.
    let planted = "let all = pool.keys(\"*\").await?;";
    let (call, _why) = FORBIDDEN_CALLS[0];
    assert!(
        planted.contains(call),
        "the forbidden-call matcher must recognize a real call site"
    );
}

#[test]
fn the_scan_tolerates_the_lua_keys_global_and_keyspace_identifiers() {
    // The three legitimate uses of the letters `KEYS` in this crate, pinned so a
    // future tightening of the matcher back to a bare word match fails here
    // rather than in whichever module it breaks first.
    for legitimate in [
        "redis.call('HSET', KEYS[1], 'v', ARGV[1])",
        "pub const REQUIRED_KEYSPACE_FLAGS: &str = \"Kxe\";",
        "cluster.provider.keyspace_notifications_set",
    ] {
        for (call, _why) in FORBIDDEN_CALLS {
            assert!(
                !legitimate.contains(call),
                "`{legitimate}` is a legitimate use and must not match `{call}`"
            );
        }
    }
}
