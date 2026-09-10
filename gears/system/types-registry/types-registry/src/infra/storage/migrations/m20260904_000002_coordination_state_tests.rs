//! Cross-dialect checks for the coordination-state migration.

use super::{DOWN_STATEMENTS, MYSQL_UP_STATEMENTS, PG_UP_STATEMENTS, SQLITE_UP_STATEMENTS};

const TABLE: &str = "types_registry__coordination_state";

/// The only state seeded in P0.
const P0_STATE: &str = "entity_write_order";

fn lists() -> [(&'static str, &'static [&'static str]); 3] {
    [
        ("postgres", PG_UP_STATEMENTS),
        ("sqlite", SQLITE_UP_STATEMENTS),
        ("mysql", MYSQL_UP_STATEMENTS),
    ]
}

/// Re-running must tolerate an existing table.
#[test]
fn every_backend_creates_the_table_only_if_absent() {
    for (name, statements) in lists() {
        let create = statements
            .iter()
            .find(|s| s.contains("CREATE TABLE"))
            .unwrap_or_else(|| panic!("{name} creates the table"));
        assert!(
            create.contains(&format!("CREATE TABLE IF NOT EXISTS {TABLE}")),
            "{name} must guard the create",
        );
    }
}

/// All dialects expose the same columns and constraints.
#[test]
fn every_backend_declares_the_same_logical_columns() {
    for (name, statements) in lists() {
        let create = statements
            .iter()
            .find(|s| s.contains("CREATE TABLE"))
            .unwrap_or_else(|| panic!("{name} creates the table"));
        for column in ["state_name", "state_seq", "updated_at"] {
            assert!(
                create.contains(column),
                "{name} declares {column}, got {create}",
            );
        }
        assert!(
            create.contains("pk_tr_coordination_state"),
            "{name} must name the primary key",
        );
        assert!(
            create.contains("ck_tr_coordination_state_seq")
                && create.contains("CHECK (state_seq >= 0)"),
            "{name} must keep the sequence non-negative",
        );
    }
}

/// The seed is idempotent and preserves existing state.
#[test]
fn every_backend_seeds_entity_write_order_idempotently() {
    for (name, statements) in lists() {
        let insert = statements
            .iter()
            .find(|s| s.contains("INSERT"))
            .unwrap_or_else(|| panic!("{name} seeds the row"));
        assert!(
            insert.contains(&format!("('{P0_STATE}', 0,")),
            "{name} seeds {P0_STATE} at sequence 0 with its timestamp, got {insert}",
        );
        assert!(
            insert.contains("updated_at"),
            "{name} stamps the seed, got {insert}",
        );
        let guarded = match name {
            "postgres" => insert.contains("ON CONFLICT DO NOTHING"),
            "sqlite" => insert.contains("INSERT OR IGNORE"),
            "mysql" => {
                insert.contains("ON DUPLICATE KEY UPDATE")
                    && insert.contains("UTC_TIMESTAMP(6)")
                    && insert.contains("state_seq = state_seq")
                    && insert.contains("updated_at = updated_at")
            }
            _ => panic!("unknown backend {name}"),
        };
        assert!(
            guarded,
            "{name} must absorb a conflict on the seed with a narrow no-op, got {insert}"
        );
    }
}

/// `routing` remains reserved for federation.
#[test]
fn p0_seeds_only_entity_write_order() {
    for (name, statements) in lists() {
        let insert = statements
            .iter()
            .find(|s| s.contains("INSERT"))
            .unwrap_or_else(|| panic!("{name} seeds the row"));
        assert!(
            !insert.contains("'routing'"),
            "{name} must not seed the federation routing state, got {insert}",
        );
        assert_eq!(
            insert.matches(", 0,").count(),
            1,
            "{name} seeds exactly one row, got {insert}",
        );
    }
}

/// Routing will use this table, not a standalone one.
#[test]
fn no_backend_creates_a_standalone_routing_table() {
    for (name, statements) in lists() {
        let sql = statements.join("\n");
        assert!(
            !sql.contains("routing_config"),
            "{name} creates a routing_config table, which will never exist",
        );
    }
}

#[test]
fn down_drops_only_this_table() {
    assert_eq!(DOWN_STATEMENTS, [format!("DROP TABLE IF EXISTS {TABLE}")]);
}
