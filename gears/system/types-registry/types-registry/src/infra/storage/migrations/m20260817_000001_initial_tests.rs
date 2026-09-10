//! Per-backend statement-list tests. `SQLite` is exercised for real by
//! `tests/migration_test.rs`; Postgres and `MySQL` need a container, so what is
//! pinned here is what a container run cannot tell us cheaply and what silently
//! rots otherwise: that all three lists cover the same tables and the same four
//! indexes, that neither federation table leaks in, and that each dialect's
//! load-bearing lowering choices are actually present.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::{DOWN_STATEMENTS, MYSQL_UP_STATEMENTS, PG_UP_STATEMENTS, SQLITE_UP_STATEMENTS};

/// The tables of the P0 subset (SPEC §9), in creation order.
const P0_TABLES: &[&str] = &[
    "types_registry__version_family",
    "types_registry__operation",
    "types_registry__operation_item",
    "types_registry__entity",
    "types_registry__type_schema_revision",
    "types_registry__instance_revision",
    "types_registry__type_schema",
    "types_registry__instance",
    "types_registry__dependency",
];

/// Every index `database.sql` declares inside the P0 subset.
const P0_INDEXES: &[&str] = &[
    "idx_tr_operation_status",
    "idx_tr_entity_family",
    "idx_tr_entity_visibility",
    "idx_tr_dependency_to",
];

/// Identifier and pattern columns, which must carry binary collation on every
/// backend so prefix ranges are exact: `version_family.family_key`,
/// `entity.gts_id` and `operation_item.gts_id`.
const IDENTIFIER_COLUMNS: &[&str] = &["family_key", "entity.gts_id", "operation_item.gts_id"];

fn lists() -> [(&'static str, &'static [&'static str]); 3] {
    [
        ("postgres", PG_UP_STATEMENTS),
        ("sqlite", SQLITE_UP_STATEMENTS),
        ("mysql", MYSQL_UP_STATEMENTS),
    ]
}

fn joined(statements: &[&str]) -> String {
    statements.join("\n")
}

#[test]
fn every_backend_creates_all_p0_tables() {
    for (name, statements) in lists() {
        let sql = joined(statements);
        for table in P0_TABLES {
            let create = format!("CREATE TABLE IF NOT EXISTS {table} (");
            assert!(sql.contains(&create), "{name} does not create {table}");
        }
    }
}

#[test]
fn every_backend_declares_all_four_p0_indexes() {
    for (name, statements) in lists() {
        let sql = joined(statements);
        for index in P0_INDEXES {
            assert!(sql.contains(index), "{name} does not declare {index}");
        }
    }
}

#[test]
fn no_backend_creates_the_federation_tables() {
    for (name, statements) in lists() {
        let sql = joined(statements);
        for table in [
            // federation, never in P0
            "types_registry__source_claim",
            // never created anywhere: its state became the `routing` row of
            // `coordination_state`
            "types_registry__routing_config",
        ] {
            assert!(
                !sql.contains(table),
                "{name} creates {table}, which this migration must not",
            );
        }
    }
}

#[test]
fn down_drops_exactly_the_p0_tables_in_reverse_creation_order() {
    let dropped: Vec<&str> = DOWN_STATEMENTS
        .iter()
        .map(|s| {
            s.strip_prefix("DROP TABLE IF EXISTS ")
                .expect("every down statement is a guarded DROP TABLE")
        })
        .collect();
    let mut expected: Vec<&str> = P0_TABLES.to_vec();
    expected.reverse();
    assert_eq!(dropped, expected);
}

// ---------------------------------------------------------------------------
// Dialect-specific lowering.
// ---------------------------------------------------------------------------

/// `family_key`, `entity.gts_id` and `operation_item.gts_id` are the three
/// identifier columns of the P0 subset. Each must be 1024 wide and binary
/// collated so pattern and derivation prefix ranges compile to exact bounds.
#[test]
fn every_backend_binary_collates_all_three_identifier_columns() {
    for (name, statements, expected) in [
        ("postgres", PG_UP_STATEMENTS, "varchar(1024) COLLATE \"C\""),
        ("sqlite", SQLITE_UP_STATEMENTS, "COLLATE BINARY"),
        (
            "mysql",
            MYSQL_UP_STATEMENTS,
            "VARCHAR(1024) CHARACTER SET ascii COLLATE ascii_bin",
        ),
    ] {
        assert_eq!(
            joined(statements).matches(expected).count(),
            IDENTIFIER_COLUMNS.len(),
            "{name} must declare every identifier column as `{expected}`"
        );
    }
}

/// With `explicit_defaults_for_timestamp` off, the first `TIMESTAMP` column of a
/// `MySQL` table silently gains `DEFAULT CURRENT_TIMESTAMP ON UPDATE
/// CURRENT_TIMESTAMP`, which would rewrite `created_at` on every update.
#[test]
fn mysql_uses_datetime_not_timestamp() {
    let sql = joined(MYSQL_UP_STATEMENTS);
    assert!(
        !sql.contains("TIMESTAMP"),
        "MySQL must use DATETIME(6); TIMESTAMP carries an implicit ON UPDATE clause"
    );
    assert!(sql.contains("DATETIME(6)"));
}

/// Neither backend has a boolean type, so the three `boolean NOT NULL` columns
/// need an explicit domain CHECK to keep `NOT dry_run` meaningful inside
/// `ck_tr_operation_item_state`.
#[test]
fn boolean_columns_are_domain_constrained_where_the_backend_has_no_boolean() {
    for (name, statements) in [
        ("sqlite", SQLITE_UP_STATEMENTS),
        ("mysql", MYSQL_UP_STATEMENTS),
    ] {
        let sql = joined(statements);
        for constraint in [
            "ck_tr_operation_dry_run_bool",
            "ck_tr_operation_item_dry_run_bool",
            "ck_tr_type_schema_revision_compat_forced_bool",
        ] {
            assert!(sql.contains(constraint), "{name} is missing {constraint}");
        }
    }
    // Postgres has the type, so it must NOT carry the lowering artifact.
    let pg = joined(PG_UP_STATEMENTS);
    assert!(
        !pg.contains("_bool CHECK"),
        "postgres needs no boolean-domain CHECK"
    );
    let normalized = pg.split_whitespace().collect::<Vec<_>>().join(" ");
    assert_eq!(
        normalized.matches("boolean NOT NULL").count(),
        3,
        "Postgres must retain the three non-null boolean columns regardless of alignment"
    );
}

/// The composite FK is what keeps an item's copied `kind` / `dry_run` in step
/// with its parent, and it needs an explicitly unique target on all three.
#[test]
fn every_backend_carries_the_composite_operation_item_foreign_key() {
    for (name, statements) in lists() {
        let sql = joined(statements);
        assert!(
            sql.contains("CONSTRAINT uq_tr_operation_kind_mode UNIQUE (id, kind, dry_run)"),
            "{name} is missing the composite FK target"
        );
        assert!(
            sql.contains("FOREIGN KEY (operation_id, kind, dry_run)"),
            "{name} is missing the composite FK"
        );
    }
}

#[test]
fn unsupported_backend_diagnostic_names_the_supported_set() {
    let err = super::unsupported_backend(sea_orm::DatabaseBackend::Postgres);
    assert!(format!("{err}").contains("Postgres, SQLite and MySQL only"));
}

#[test]
fn each_supported_backend_dispatches_to_its_own_statement_list() {
    for (backend, expected) in [
        (sea_orm::DatabaseBackend::Postgres, PG_UP_STATEMENTS),
        (sea_orm::DatabaseBackend::Sqlite, SQLITE_UP_STATEMENTS),
        (sea_orm::DatabaseBackend::MySql, MYSQL_UP_STATEMENTS),
    ] {
        let got = super::up_statements(backend).expect("supported backend");
        assert_eq!(
            got, expected,
            "{backend:?} resolved to the wrong statement list"
        );
    }
}

// ---------------------------------------------------------------------------
// Cross-dialect drift
// ---------------------------------------------------------------------------

/// Constraint names that legitimately exist on some backends and not others, each
/// with the dialect reason. Anything asymmetric that is *not* in this table is drift.
///
/// Three families, each a substitution rather than a difference in meaning:
///
/// * `*_uuid_len` — `SQLite` only. It is typeless, so a `length(x) = 16` CHECK is
///   what stands in for `uuid` / `BINARY(16)`.
/// * `*_bool` — `SQLite` and `MySQL`. Neither has a native boolean, so `IN (0, 1)`
///   stands in for `boolean`.
/// * `pk_*` on the three surrogate-key tables — `PostgreSQL` and `MySQL` only.
///   `SQLite` spells that primary key inline as `INTEGER PRIMARY KEY AUTOINCREMENT`,
///   which is the one form that gives `rowid` aliasing, and an inline primary key
///   cannot carry a name.
const DIALECT_SPECIFIC: &[(&str, &[&str])] = &[
    ("ck_tr_entity_uuid_len", &["sqlite"]),
    ("ck_tr_operation_uuid_len", &["sqlite"]),
    ("ck_tr_operation_item_uuid_len", &["sqlite"]),
    ("ck_tr_version_family_owner_uuid_len", &["sqlite"]),
    ("ck_tr_operation_dry_run_bool", &["sqlite", "mysql"]),
    ("ck_tr_operation_item_dry_run_bool", &["sqlite", "mysql"]),
    (
        "ck_tr_type_schema_revision_compat_forced_bool",
        &["sqlite", "mysql"],
    ),
    ("pk_tr_version_family", &["postgres", "mysql"]),
    ("pk_tr_operation_item", &["postgres", "mysql"]),
    ("pk_tr_entity", &["postgres", "mysql"]),
];

/// Every `CONSTRAINT <name>` in one dialect's statements, in declaration order with
/// duplicates collapsed.
fn constraint_names(statements: &[&str]) -> Vec<String> {
    let mut names = Vec::new();
    for statement in statements {
        for tail in statement.split("CONSTRAINT ").skip(1) {
            let name: String = tail
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() && !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names
}

/// The standing half of the report's one-time "48 constraints for 48" count.
///
/// A constraint added to one dialect's copy and forgotten in the others is the
/// failure mode three hand-maintained DDL copies actually have, and no `SQLite` test
/// can see it: the other two lists are never executed without a container. So the
/// three name sets are compared against each other, and every asymmetry must be a
/// declared dialect substitution — which turns "someone remembered" into "CI checks".
#[test]
fn no_constraint_exists_on_one_backend_and_not_the_others() {
    let per_backend: Vec<(&str, Vec<String>)> = lists()
        .into_iter()
        .map(|(name, statements)| (name, constraint_names(statements)))
        .collect();

    let mut all: Vec<String> = Vec::new();
    for (_, names) in &per_backend {
        for name in names {
            if !all.contains(name) {
                all.push(name.clone());
            }
        }
    }
    all.sort();

    for name in &all {
        let mut present: Vec<&str> = per_backend
            .iter()
            .filter(|(_, names)| names.contains(name))
            .map(|(backend, _)| *backend)
            .collect();
        if present.len() == per_backend.len() {
            continue;
        }
        let declared = DIALECT_SPECIFIC
            .iter()
            .find(|(declared, _)| declared == name)
            .map(|(_, backends)| *backends);
        let Some(expected) = declared else {
            panic!(
                "constraint '{name}' is declared only on {present:?}. Either add it to \
                 the other dialects' statements, or — if it is a dialect substitution — \
                 add it to DIALECT_SPECIFIC with the reason."
            );
        };
        let mut expected: Vec<&str> = expected.to_vec();
        expected.sort_unstable();
        present.sort_unstable();
        assert_eq!(
            present, expected,
            "'{name}' is declared on a different set of backends than DIALECT_SPECIFIC says",
        );
    }
}

/// The other direction: an entry in [`DIALECT_SPECIFIC`] that no longer names a
/// dialect-specific constraint. Otherwise the table would quietly become a list of
/// things that used to be true, and the guard above would exempt names that are no
/// longer exceptions at all.
#[test]
fn every_declared_dialect_exception_is_still_one() {
    let per_backend: Vec<(&str, Vec<String>)> = lists()
        .into_iter()
        .map(|(name, statements)| (name, constraint_names(statements)))
        .collect();

    for (name, backends) in DIALECT_SPECIFIC {
        let present: Vec<&str> = per_backend
            .iter()
            .filter(|(_, names)| names.iter().any(|n| n == name))
            .map(|(backend, _)| *backend)
            .collect();
        assert!(
            !present.is_empty(),
            "DIALECT_SPECIFIC names '{name}', which no dialect declares any more",
        );
        assert_ne!(
            present.len(),
            per_backend.len(),
            "'{name}' is now on every backend, so it is no longer an exception",
        );
        assert_eq!(present.len(), backends.len(), "'{name}': {present:?}");
    }
}

/// The count itself, as a tripwire for a whole table going missing from one copy —
/// which the name comparison above would report as dozens of separate failures.
#[test]
fn the_three_dialects_declare_the_counts_they_are_expected_to() {
    for (name, statements) in lists() {
        let count = constraint_names(statements).len();
        let expected = match name {
            "postgres" => 48,
            "sqlite" => 52,
            "mysql" => 51,
            other => panic!("unknown backend {other}"),
        };
        assert_eq!(
            count, expected,
            "{name} declares {count} named constraints, expected {expected} — if this is \
             a deliberate schema change, the other two dialects and this number move together",
        );
    }
}
