//! The Postgres half of the schema oracle.
//!
//! The same three properties its `SQLite` sibling establishes — deterministic, complete, and
//! honest about what it normalises — plus the golden the re-authoring is checked against. See
//! `sqlite_schema_dump.rs` for the argument; this file only states what differs on this engine.
//!
//! **What differs.** Postgres re-renders constraints and indexes from their parsed form, so
//! `pg_get_constraintdef` returns the server's canonical spelling rather than the submitted text.
//! A re-authored migration that writes the same rule differently therefore compares equal here on
//! the server's own authority, which is a stronger guarantee than the `SQLite` side can give. The
//! dump also carries functions, which `SQLite` has none of, and `EXCLUDE` constraints, which
//! `SQLite` expresses as trigger pairs.
//!
//! Task 2 of `tasks/plan.md`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod pg_support;
mod schema_dump;

use schema_dump::postgres_dump;

/// Every `pricing_` table the chain leaves standing, counted from the dump's `COLUMN` lines.
///
/// The same number the `SQLite` half asserts, and asserted here for the same reason: an
/// over-eager filter produces a dump that is perfectly deterministic and perfectly useless.
const PRICING_TABLES: usize = 41;

fn tables_in(dump: &str) -> Vec<String> {
    let mut names: Vec<String> = dump
        .lines()
        .filter_map(|line| line.strip_prefix("COLUMN "))
        .filter_map(|rest| rest.split_whitespace().next())
        .map(str::to_owned)
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Two databases, the same chain, the same dump.
///
/// Two databases rather than two reads of one: `Pg::applied` creates a fresh database and runs the
/// chain into it, so this asks whether the *chain* is deterministic. A migration that seeded a
/// generated name, a timestamp or an oid-dependent default into a constraint would pass the
/// weaker version of this case and fail this one.
#[tokio::test]
#[ignore = "needs the Postgres harness"]
async fn the_dump_of_one_chain_is_the_same_twice() {
    let first = postgres_dump(&pg_support::Pg::applied().await.raw().await).await;
    let second = postgres_dump(&pg_support::Pg::applied().await.raw().await).await;

    assert_eq!(
        first, second,
        "two runs of the same chain produced different dumps"
    );
    assert!(
        !first.is_empty(),
        "an empty dump is deterministic and worthless"
    );
}

/// The dump reaches every kind of object this engine carries.
///
/// Each kind is asserted separately rather than by a single total, because a query that returned
/// nothing would otherwise be hidden by the other four. Functions are the sharpest of them: they
/// exist only on this engine, so nothing else in the suite would notice their absence here.
#[tokio::test]
#[ignore = "needs the Postgres harness"]
async fn the_dump_reaches_every_kind_of_object() {
    let dump = postgres_dump(&pg_support::Pg::applied().await.raw().await).await;

    let tables = tables_in(&dump);
    let pricing: Vec<&String> = tables
        .iter()
        .filter(|name| name.contains(".pricing_"))
        .collect();
    assert_eq!(
        pricing.len(),
        PRICING_TABLES,
        "the dump names {} pricing tables, expected {PRICING_TABLES}: {pricing:?}",
        pricing.len()
    );

    for kind in ["COLUMN ", "CONSTRAINT ", "INDEX ", "TRIGGER ", "FUNCTION "] {
        assert!(
            dump.lines().any(|line| line.starts_with(kind)),
            "no {kind}line reached the dump; that query returned nothing"
        );
    }

    // The `EXCLUDE` added by `pricing_price_window` is the one constraint kind `SQLite` cannot
    // express, so it is the one the two goldens can never agree about and the one a Postgres-only
    // oracle exists to watch.
    assert!(
        dump.contains("EXCLUDE USING gist"),
        "the window non-overlap exclusion constraint is not in the dump"
    );

    // Objects belong in `bss`. A `public` object is not necessarily wrong -- the runner's own
    // history table lives there -- but it is excluded from this dump, so anything left in
    // `public` is something the chain put there and is worth seeing named.
    //
    // **Matched on each kind's own rendered shape.** `COLUMN`, `CONSTRAINT` and `TRIGGER`
    // render `<schema>.<relation>` and the dotted arm reads them; `INDEX` and `FUNCTION`
    // separate the schema with a space and need their own. `FUNCTION public` does reach the
    // dotted arm through `pg_get_functiondef`'s own qualification of the body, which is an
    // incidental match on a second rendering rather than on the discriminator, and this arm
    // is what makes it the discriminator. `EXTENSION` carries no schema at all: an extension
    // is not schema-scoped here and `btree_gist` living in `public` is the arrangement
    // `m20260821_000002_install_btree_gist` intends.
    let stray: Vec<&str> = dump
        .lines()
        .filter(|line| {
            line.contains(" public.")
                || line.starts_with("INDEX public ")
                || line.starts_with("FUNCTION public ")
        })
        .collect();
    assert!(
        stray.is_empty(),
        "the chain left objects outside the bss schema: {stray:?}"
    );
}

/// The frozen dump: does today's chain still produce the schema we recorded?
///
/// Re-record with `UPDATE_SCHEMA_GOLDEN=1`. Doing so is a claim that the schema was **meant** to
/// change, and the diff this case prints is what has to justify it.
#[tokio::test]
#[ignore = "needs the Postgres harness"]
async fn the_chain_still_produces_the_frozen_schema() {
    let golden_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/schema_golden/postgres.txt"
    );
    let fresh = postgres_dump(&pg_support::Pg::applied().await.raw().await).await;

    if std::env::var("UPDATE_SCHEMA_GOLDEN").is_ok() {
        let dir = std::path::Path::new(golden_path)
            .parent()
            .expect("the golden lives in a directory");
        std::fs::create_dir_all(dir).expect("create the golden directory");
        std::fs::write(golden_path, &fresh).expect("write the golden");
        return;
    }

    let golden = std::fs::read_to_string(golden_path).unwrap_or_else(|e| {
        panic!("no frozen schema at {golden_path} ({e}); record one with UPDATE_SCHEMA_GOLDEN=1")
    });

    if golden == fresh {
        return;
    }

    // Each line already names its own object, so the first differing line is the report. That is
    // why this dump is one line per object rather than the stanzas the `SQLite` side needs.
    for (n, (want, got)) in golden.lines().zip(fresh.lines()).enumerate() {
        assert!(
            want == got,
            "the schema changed at line {}:\n  frozen: {want}\n  now:    {got}",
            n + 1
        );
    }
    panic!(
        "the schema changed in length only: frozen has {} lines, now {}",
        golden.lines().count(),
        fresh.lines().count()
    );
}
