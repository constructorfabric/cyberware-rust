//! Live `PostgreSQL` 19 tests.
//!
//! SQL/PGQ exists on no other engine and on no earlier major, so these cannot
//! live beside the `SQLite` suite. The image comes from `cf-gears-test-containers`
//! (`POSTGRES_GRAPH_TAG`); while `PostgreSQL` 19 is pre-GA an unavailable image is
//! a skip, and `GEARS_TEST_PG_GRAPH_REQUIRED=1` turns it into a failure so a CI
//! lane meant to cover PG19 cannot pass vacuously.

#![cfg(feature = "pgq")]

mod secure_graph;
