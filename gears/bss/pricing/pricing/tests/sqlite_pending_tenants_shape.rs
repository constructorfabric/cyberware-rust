//! The **shape** of the read `catalog_version_ref_repo::pending_tenants` issues,
//! counted rather than reasoned about.
//!
//! The warm sweep opens every pass with this call, at `readmodel_warm_tick_secs`
//! (5s by default), and enumerates up to `pending_tenants_per_pass` (250) distinct
//! tenant ids. It did so as a loose index scan: one `find().limit(1)` per tenant
//! found, each asking for the smallest tenant id above the last. So a deployment
//! with 250 tenants mid-publish paid 250 round trips every five seconds *before*
//! the two pending reads per tenant and the registry calls — the largest single
//! contributor to the pass outrunning the 5-second lease TTL it holds.
//!
//! # Why this is a whole test binary of its own, with exactly one test in it
//!
//! The claim is "how many statements", and nothing in the repository surface
//! reports that. `DBRunner` is sealed, so a counting runner cannot be written;
//! `sqlx` logs every statement it executes as a `tracing` event on the
//! `sqlx::query` target, and that is the only observation point. It has to be a
//! **global** subscriber — `sqlx_sqlite` runs each statement on the connection's
//! own worker thread, so a thread-local dispatcher installed by the test sees
//! nothing — and a global counter is fed by whatever else runs in parallel. One
//! test, one process, one counter: the first draft of this file had three tests and
//! the counted one read **17** where the shape it measured issues 13, because the
//! sibling cases' own reads landed in the same counter.
//! `tests/sqlite_overlay_world_fanout.rs` is the same arrangement for the same
//! reason and this file follows it. The three claims are therefore three **phases**
//! of one case, the counted one first.
//!
//! # Why twelve tenants and not two
//!
//! A probe over one or two tenants cannot tell the two shapes apart: the paged read
//! issues one statement for a page and a second to learn the page was the last,
//! which is also what the per-tenant walk spends on two tenants. Twelve makes the
//! difference unmistakable — thirteen statements for the walk (one per tenant plus
//! the empty one that ends it) against two for the page — and the assertion is
//! written against the seeded tenant count rather than against a literal, so
//! growing the seed cannot make it agree with the shape it refuses.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bss_pricing::domain::read_model::SubjectRef;
use bss_pricing::infra::storage::migrations::Migrator;
use bss_pricing::infra::storage::repo::{PendingVersionRow, catalog_version_ref_repo};

use bss_pricing::domain::instant::utc_ymd_hms;
use sea_orm_migration::MigratorTrait;
use time::OffsetDateTime;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::AccessScope;
use toolkit_db::secure::DBRunner;
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};
use uuid::Uuid;

/// Tenants holding a pending ref. Read back into the assertions, so it is not a
/// literal the probe compares against itself.
const TENANTS: usize = 12;

/// Counts the `SELECT`s `sqlx` logs against `pricing_catalog_version_ref`.
struct StatementCounter {
    ref_statements: Arc<AtomicUsize>,
}

impl Subscriber for StatementCounter {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.target() == "sqlx::query"
    }

    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut sql = StatementText(String::new());
        event.record(&mut sql);
        // `SELECT`s only: the seed's own INSERTs name the same table and would
        // otherwise be counted as reads.
        if sql.0.contains("pricing_catalog_version_ref") && sql.0.contains("SELECT") {
            self.ref_statements.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}

/// Every field of the event, concatenated — `sqlx` puts the statement in
/// `summary` when it is short and in `db.statement` when it is not, and renders
/// `String` fields through either `record_str` or `record_debug` depending on the
/// `tracing` version. Reading all of them means the probe does not rest on which.
struct StatementText(String);

impl Visit for StatementText {
    fn record_str(&mut self, _: &Field, value: &str) {
        self.0.push_str(value);
    }

    // `Visit`'s one **required** method renders through `Debug`, and a field the
    // count depends on may arrive that way. This buffer is never printed, only
    // searched for a table name.
    #[allow(clippy::use_debug, clippy::let_underscore_must_use)]
    fn record_debug(&mut self, _: &Field, value: &dyn std::fmt::Debug) {
        let _ = write!(self.0, "{value:?}");
    }
}

fn at(minute: u32) -> OffsetDateTime {
    utc_ymd_hms(2099, 1, 1, 9, minute, 0)
}

/// Tenant ids that ascend with `n`, so the walk's ordering claim is checkable.
fn tenant(n: usize) -> Uuid {
    Uuid::from_u128(0x1000_0000 + u128::try_from(n).expect("small"))
}

async fn seed_ref(conn: &impl DBRunner, scope: &AccessScope, tenant_id: Uuid, handle: &str) {
    catalog_version_ref_repo::record_pending(
        conn,
        scope,
        PendingVersionRow::for_subject(
            tenant_id,
            handle.to_owned(),
            &SubjectRef::Plan(Uuid::new_v4()),
            Some(0),
            None,
            at(0),
        ),
    )
    .await
    .expect("record a pending ref");
}

#[tokio::test]
async fn the_tenant_discovery_read_pages_rather_than_seeking_once_per_tenant() {
    let ref_statements = Arc::new(AtomicUsize::new(0));
    tracing::subscriber::set_global_default(StatementCounter {
        ref_statements: Arc::clone(&ref_statements),
    })
    .expect("this binary installs the only subscriber");

    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("run migrator");
    let provider = DBProvider::<DbError>::new(db);
    let conn = provider.conn().expect("conn");
    let scope = AccessScope::allow_all();

    let mut seeded: Vec<Uuid> = Vec::new();
    for n in 0..TENANTS {
        let id = tenant(n);
        seeded.push(id);
        seed_ref(&conn, &scope, id, &format!("pend-{n}")).await;
    }

    // ---- Phase 1: the shape, counted. -------------------------------------
    ref_statements.store(0, Ordering::Relaxed);
    let found = catalog_version_ref_repo::pending_tenants(
        &conn,
        &scope,
        u64::try_from(TENANTS).expect("small"),
    )
    .await
    .expect("the discovery read succeeds");
    let statements = ref_statements.load(Ordering::Relaxed);

    // The counter is armed: if the mechanism observed nothing this is what says
    // so, rather than the count silently agreeing with a paged shape it never
    // measured.
    assert!(
        statements > 0,
        "the statement counter observed no read at all, so it is measuring nothing"
    );
    // The answer first. A paging change that lost tenants would otherwise satisfy
    // the count assertion below by reading less.
    assert_eq!(
        found, seeded,
        "every tenant holding a pending ref must be found, in ascending tenant id"
    );
    assert!(
        statements <= 2,
        "the discovery read must page: it took {statements} statements for {TENANTS} tenants, \
         where one seek per tenant is {TENANTS} — the shape this suite exists to refuse (and \
         {} once the bound is not what stops the walk)",
        TENANTS + 1
    );

    // ---- Phase 2: the bound still truncates the tail. ----------------------
    // The positive control on the refusal above: paging must not turn
    // `pending_tenants_per_pass` into a suggestion. D-163 hands the number and the
    // order to the owner and says the tail past the bound is not swept at all, so
    // a page larger than the bound has to be cut back to it — and cut back at the
    // **end** of the ascending order, not wherever a page happened to break.
    let bounded = catalog_version_ref_repo::pending_tenants(&conn, &scope, 4)
        .await
        .expect("the discovery read succeeds");
    assert_eq!(
        bounded,
        vec![tenant(0), tenant(1), tenant(2), tenant(3)],
        "the bound admits the first four of the ascending order and nothing beyond it"
    );

    // ---- Phase 3: a page is rows, and the answer is tenants. ---------------
    // Four more refs on each of the first two tenants, so a page of any size from
    // two to nine breaks **inside** one tenant's rows. Deduplicating in memory is
    // what makes the answer distinct tenants rather than rows; a break inside a
    // tenant must not report it twice nor skip the tenant after it.
    for extra in 1..5_usize {
        seed_ref(&conn, &scope, tenant(0), &format!("pend-0-{extra}")).await;
        seed_ref(&conn, &scope, tenant(1), &format!("pend-1-{extra}")).await;
    }
    let deduped = catalog_version_ref_repo::pending_tenants(&conn, &scope, 250)
        .await
        .expect("the discovery read succeeds");
    assert_eq!(
        deduped, seeded,
        "twenty rows over twelve tenants are twelve tenants, each named once and none skipped"
    );
}
