//! The pin-eligibility frontier is **forward only**, and its advance belongs to
//! the caller's transaction — against a real database, not a mock.
//!
//! Forward-only is the property the whole materialization exists for (D-136).
//! If the watermark could recede, a consumer pinning it would resolve two
//! different contents at the same version over time, which is precisely the
//! divergence D-101/D-114 close and which recomputing the predicate at read
//! time could not afford. A mock would only assert that the repository's own
//! `if` fires; the guard that has to hold under concurrency is the
//! `catalog_version < :to` predicate on the UPDATE, and only a database
//! evaluates that.
//!
//! The transaction half is the property the runner-taking move exists to buy.
//! D-136 requires the advance to run in the transaction that sets the last
//! outstanding `warm_completed` marker of the frontier's next version, so a
//! frontier that survived that transaction's rollback would mark a version
//! pin-eligible whose delta rows were never written — a pin resolving nothing.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use bss_pricing::domain::error::DomainError;
use bss_pricing::infra::storage::migrations::Migrator;
use bss_pricing::infra::storage::repo::{PinFrontierRepo, pin_frontier_repo};
use bss_pricing::infra::storage::{RepoError, repo_failure};
use bss_pricing_sdk::CatalogVersion;

use bss_pricing::domain::instant::utc_ymd_hms;
use sea_orm_migration::MigratorTrait;
use time::OffsetDateTime;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::{AccessScope, TxError};
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

async fn provider() -> DBProvider<DbError> {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("run migrator");
    DBProvider::<DbError>::new(db)
}

/// Advance through a plain connection — what every case here that is not about
/// transactions needs, now that the advance takes a runner.
async fn advance(
    provider: &DBProvider<DbError>,
    scope: &AccessScope,
    tenant: Uuid,
    to: CatalogVersion,
    at: OffsetDateTime,
) -> Result<(), RepoError> {
    let conn = provider.conn().expect("conn");
    pin_frontier_repo::advance(&conn, scope, tenant, to, at).await
}

#[tokio::test]
async fn the_frontier_only_ever_moves_forward() {
    let provider = provider().await;
    let repo = PinFrontierRepo::new(provider.clone());
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let t1 = utc_ymd_hms(2026, 8, 2, 10, 0, 0);
    let t2 = utc_ymd_hms(2026, 8, 2, 10, 0, 5);
    let t3 = utc_ymd_hms(2026, 8, 2, 10, 0, 9);

    // No frontier yet: a consumer has nothing it may pin, and fails closed.
    assert_eq!(
        repo.read(&scope, tenant).await.expect("read"),
        None,
        "a tenant with no completed version has no frontier"
    );

    // First advance inserts the row.
    advance(&provider, &scope, tenant, CatalogVersion::new(1), t1)
        .await
        .expect("advance to 1");
    let frontier = repo
        .read(&scope, tenant)
        .await
        .expect("read")
        .expect("frontier present after the first advance");
    assert_eq!(frontier.catalog_version, CatalogVersion::new(1));
    assert_eq!(frontier.advanced_at, t1);

    // 1 -> 5 moves it.
    advance(&provider, &scope, tenant, CatalogVersion::new(5), t2)
        .await
        .expect("advance to 5");
    let frontier = repo
        .read(&scope, tenant)
        .await
        .expect("read")
        .expect("frontier present");
    assert_eq!(frontier.catalog_version, CatalogVersion::new(5));
    assert_eq!(frontier.advanced_at, t2);

    // 5 -> 3 does NOT move it, and says so.
    let err = advance(&provider, &scope, tenant, CatalogVersion::new(3), t3)
        .await
        .expect_err("a backwards advance must be refused");
    assert_eq!(
        err,
        RepoError::FrontierRegression {
            tenant: tenant.to_string(),
            current: 5,
            requested: 3,
        }
    );

    // 5 -> 5 is refused too: re-advancing to the standing version means the
    // projector completed the same version twice, which is an ordering bug, not
    // a harmless retry.
    let err = advance(&provider, &scope, tenant, CatalogVersion::new(5), t3)
        .await
        .expect_err("an equal advance must be refused");
    assert!(matches!(err, RepoError::FrontierRegression { .. }));

    // Neither refusal moved anything, including the timestamp.
    let frontier = repo
        .read(&scope, tenant)
        .await
        .expect("read")
        .expect("frontier present");
    assert_eq!(frontier.catalog_version, CatalogVersion::new(5));
    assert_eq!(
        frontier.advanced_at, t2,
        "a refused advance must not restamp advanced_at"
    );
}

#[tokio::test]
async fn a_foreign_tenants_frontier_is_invisible() {
    let provider = provider().await;
    let repo = PinFrontierRepo::new(provider.clone());
    let mine = Uuid::from_u128(0x7e_11);
    let theirs = Uuid::from_u128(0x7e_22);
    let at = utc_ymd_hms(2026, 8, 2, 10, 0, 0);

    advance(
        &provider,
        &AccessScope::for_tenant(theirs),
        theirs,
        CatalogVersion::new(9),
        at,
    )
    .await
    .expect("advance the other tenant");

    // SQL-level BOLA: my scope sees no row of theirs, whichever tenant id I ask
    // for. The read model is commercially sensitive, so this fails to `None`
    // rather than to someone else's frontier.
    assert_eq!(
        repo.read(&AccessScope::for_tenant(mine), theirs)
            .await
            .expect("read"),
        None
    );
}

#[tokio::test]
async fn an_advance_inside_a_callers_transaction_lands() {
    // The property the runner-taking move exists to make possible at all: the
    // old method acquired its own connection, which inside an open transaction
    // is refused outright by the toolkit's transaction-bypass guard — so on the
    // only path D-136 permits the advance to run on, it could not run.
    let provider = provider().await;
    let repo = PinFrontierRepo::new(provider.clone());
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let at = utc_ymd_hms(2026, 8, 2, 10, 0, 0);

    let inner_scope = scope.clone();
    let (_, outcome) = provider
        .db()
        .in_transaction::<(), RepoError, _>(move |txn| {
            Box::pin(async move {
                // A read through the same runner, then the advance: exactly
                // what the projector does, in exactly one transaction.
                assert_eq!(
                    pin_frontier_repo::read_at(txn, &inner_scope, tenant)
                        .await
                        .expect("read inside the transaction"),
                    None
                );
                pin_frontier_repo::advance(txn, &inner_scope, tenant, CatalogVersion::new(7), at)
                    .await
            })
        })
        .await;
    outcome.expect("the advance runs inside the caller's transaction");

    assert_eq!(
        repo.read(&scope, tenant)
            .await
            .expect("read")
            .expect("frontier present")
            .catalog_version,
        CatalogVersion::new(7)
    );
}

#[tokio::test]
async fn an_advance_whose_transaction_rolls_back_leaves_the_frontier_where_it_was() {
    // This is what the advance being the caller's is *for*. D-136 puts it in
    // the transaction that sets the last outstanding warm marker; a frontier
    // that survived that transaction's rollback would declare a version
    // pin-eligible whose delta rows were never written, and a consumer pinning
    // it would resolve nothing at all.
    let provider = provider().await;
    let repo = PinFrontierRepo::new(provider.clone());
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let t1 = utc_ymd_hms(2026, 8, 2, 10, 0, 0);
    let t2 = utc_ymd_hms(2026, 8, 2, 10, 0, 5);

    advance(&provider, &scope, tenant, CatalogVersion::new(2), t1)
        .await
        .expect("advance to 2");

    let inner_scope = scope.clone();
    let (_, outcome) = provider
        .db()
        .in_transaction::<(), RepoError, _>(move |txn| {
            Box::pin(async move {
                pin_frontier_repo::advance(txn, &inner_scope, tenant, CatalogVersion::new(9), t2)
                    .await?;
                // **The advance is read back inside the transaction**, and that is
                // what makes the assertions below a rollback measurement. Without
                // it `outcome.is_err()` holds against the error this closure
                // injects itself, so it stands whether the advance landed or its
                // own `?` propagated - and a frontier still reading 2 afterwards
                // says nothing either way.
                let staged = pin_frontier_repo::read_at(txn, &inner_scope, tenant)
                    .await?
                    .expect("the frontier exists inside the transaction");
                assert_eq!(
                    staged.catalog_version,
                    CatalogVersion::new(9),
                    "the advance landed before the projection failed"
                );
                // The projection this advance was supposed to be part of fails.
                Err(RepoError::Db(
                    "the projection failed after the advance".to_owned(),
                ))
            })
        })
        .await;
    assert!(
        matches!(
            outcome,
            Err(TxError::Domain(RepoError::Db(ref detail)))
                if detail.contains("the projection failed")
        ),
        "the transaction carried the injected failure out: {outcome:?}"
    );

    let frontier = repo
        .read(&scope, tenant)
        .await
        .expect("read")
        .expect("frontier present");
    assert_eq!(
        frontier.catalog_version,
        CatalogVersion::new(2),
        "the advance went back with its transaction"
    );
    assert_eq!(frontier.advanced_at, t1);
}

#[test]
fn a_refused_advance_maps_to_a_precondition_failure() {
    // Pinned as it stands, and it contradicts D-146 — which says of this exact
    // case that "a regression there is an internal fault, not a lifecycle
    // refusal". The mapping is left alone and the contradiction reported:
    // nothing on a wire path can reach either rendering, this variant's only
    // producer being a background sweep whose errors are logged.
    let err = RepoError::FrontierRegression {
        tenant: Uuid::from_u128(1).to_string(),
        current: 5,
        requested: 3,
    };

    assert!(matches!(
        repo_failure(&err),
        DomainError::LifecycleForbidden(_)
    ));
}
