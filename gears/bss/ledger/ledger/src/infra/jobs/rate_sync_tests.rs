//! Tests for `RateSyncJob`.
//!
//! The fetch-handling branches (no adapter / configured-provider failure / empty
//! fetch) all return BEFORE the per-tenant fan-out touches the DB, so they run
//! against a bare in-memory SQLite provider + a `noop()` publisher. The
//! tenant-fan-out path reads `fiscal_calendar` and upserts `ledger_fx_rate`, so it
//! is a Docker-gated `#[ignore]` testcontainer test.
//!
//! Ignored Docker tests run with
//! `cargo test -p cf-gears-bss-ledger --lib 'infra::jobs::rate_sync::tests' -- --ignored`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown
)]

use async_trait::async_trait;
use bss_ledger_sdk::{CurrencyPair, UnconfiguredRateProviderV1};
use sea_orm::{ConnectionTrait, Database, Statement};
use sea_orm_migration::MigratorTrait;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use toolkit_db::{ConnectOpts, connect_db};

use super::*;
use crate::infra::storage::migrations::Migrator;
use time::OffsetDateTime;

/// A configurable fake `RateProviderV1`: a stable id + a fixed fetch outcome.
/// `id` is owned (not `&'static`) so `provider_id` returns a borrowed `&self.id`
/// genuinely tied to `&self` — matching how a real adapter holds its id.
struct FakeProvider {
    id: String,
    outcome: Outcome,
}

enum Outcome {
    Ok(Vec<ProviderRate>),
    Fail,
}

#[async_trait]
impl RateProviderV1 for FakeProvider {
    fn provider_id(&self) -> &str {
        &self.id
    }
    async fn fetch_latest(
        &self,
        _ctx: &SecurityContext,
        _pairs: &[CurrencyPair],
        _request_id: &str,
    ) -> Result<Vec<ProviderRate>, RateProviderError> {
        match &self.outcome {
            // Stamp this source's own id onto every rate, exactly as a real
            // source does — the fixtures deliberately carry a different
            // placeholder, so a rate reaching the store still labelled
            // `unstamped` proves the provenance was never applied.
            Outcome::Ok(rates) => Ok(rates
                .iter()
                .map(|r| ProviderRate {
                    provider: self.id.clone(),
                    ..r.clone()
                })
                .collect()),
            Outcome::Fail => Err(RateProviderError::Unreachable("fake outage".to_owned())),
        }
    }

    /// Always reachable. The sync job never calls this — it only fetches — so the
    /// fake keeps it trivially `Ok` rather than modelling an outcome no assertion
    /// here reads.
    async fn health(
        &self,
        _ctx: &SecurityContext,
        _request_id: &str,
    ) -> Result<(), RateProviderError> {
        Ok(())
    }
}

/// A `ProviderRate` literal (timestamp = now; staleness is the resolver's
/// concern, not the sync job's).
///
/// `provider` is a placeholder the serving source overwrites with its own id,
/// which is where the value the job stores actually comes from. Left distinct
/// from any fake's id on purpose: an assertion on the stored provenance then
/// fails loudly if the stamping step is ever dropped.
fn rate(base: &str, quote: &str, rate_micro: i64) -> ProviderRate {
    ProviderRate {
        base: base.to_owned(),
        quote: quote.to_owned(),
        rate_micro,
        as_of: OffsetDateTime::now_utc(),
        provider: "unstamped".to_owned(),
    }
}

/// A job over a bare in-memory SQLite provider (no migrations) + a `noop()`
/// publisher — enough for the fetch-handling branches, which return before the
/// per-tenant DB fan-out.
async fn job_no_db(provider: Arc<dyn RateProviderV1>) -> RateSyncJob {
    let db = connect_db(
        "sqlite:file:fx_rate_sync_unit?mode=memory&cache=shared",
        ConnectOpts::default(),
    )
    .await
    .unwrap();
    let dbp = DBProvider::<DbError>::new(db);
    let repo = FxRepo::new(dbp.clone());
    let publisher = Arc::new(LedgerEventPublisher::noop());
    RateSyncJob::new(dbp, provider, repo, publisher)
}

#[tokio::test]
async fn unconfigured_provider_is_inert_and_does_not_alarm() {
    // The fail-safe default (`provider_id() == "none"`) fetch-errors; the job logs
    // at debug and returns the default (unfetched) report WITHOUT alarming — a
    // missing adapter is a deployment state, not a books defect.
    let job = job_no_db(Arc::new(UnconfiguredRateProviderV1)).await;
    let report = job.run().await.expect("an inert sync is Ok");
    assert_eq!(report, RateSyncReport::default());
    assert!(!report.fetched);
}

#[tokio::test]
async fn configured_provider_failure_never_aborts() {
    // A CONFIGURED provider that fails to fetch emits FX_SNAPSHOT_MISSING (here to
    // the noop publisher) and returns Ok with `fetched = false` — a provider
    // outage must never abort the gear.
    let job = job_no_db(Arc::new(FakeProvider {
        id: "ecb".to_owned(),
        outcome: Outcome::Fail,
    }))
    .await;
    let report = job.run().await.expect("a provider outage never aborts");
    assert!(!report.fetched);
    assert_eq!(report.rates, 0);
}

#[test]
fn snapshot_missing_detail_carries_the_request_id_for_log_correlation() {
    // The alarm's own fields cannot identify the outage: `provider_id` is the
    // composite adapter's id, not the source that broke, and the error is only the
    // LAST source a composite tried. The `request_id` is what makes the tick
    // reconstructable — the adapter stamps the same id on a `warn` line per
    // attempted source, so filtering by it yields the full sequence in priority
    // order. Assert every part an operator reads, since dropping any one of them
    // leaves the alarm unactionable.
    let detail = snapshot_missing_detail(
        "fx-composite",
        "0199a3c4-1d2e-7f00-8000-000000000001",
        &RateProviderError::Unreachable("connect timed out".to_owned()),
    );
    assert!(detail.contains("fx-composite"), "{detail}");
    assert!(
        detail.contains("request_id=0199a3c4-1d2e-7f00-8000-000000000001"),
        "the alarm must carry the tick's request_id: {detail}"
    );
    assert!(detail.contains("connect timed out"), "{detail}");
}

#[tokio::test]
async fn empty_fetch_is_fetched_with_no_fanout() {
    // An empty (but successful) fetch is `fetched = true` with nothing to fan out;
    // it returns before the per-tenant DB write.
    let job = job_no_db(Arc::new(FakeProvider {
        id: "ecb".to_owned(),
        outcome: Outcome::Ok(vec![]),
    }))
    .await;
    let report = job.run().await.expect("ok");
    assert!(report.fetched);
    assert_eq!(report.rates, 0);
    assert_eq!(report.tenants, 0);
}

// ---------------------------------------------------------------------------
// Docker (testcontainers) — the per-tenant fan-out path.
// ---------------------------------------------------------------------------

fn pg(sql: impl Into<String>) -> Statement {
    Statement::from_string(sea_orm::DatabaseBackend::Postgres, sql.into())
}

/// A successful fetch fans every published rate out to EVERY provisioned tenant's
/// `ledger_fx_rate` store, stamped with the provider id, and reports the counts.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn fans_rates_out_to_every_provisioned_tenant() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let raw = Database::connect(&url).await.unwrap();
    Migrator::up(&raw, None).await.unwrap();

    let repo_url = format!("{url}?options=-c%20search_path%3Dbss,public");
    let dbp =
        DBProvider::<DbError>::new(connect_db(&repo_url, ConnectOpts::default()).await.unwrap());

    // Two provisioned tenants (each a fiscal_calendar row — the same provisioned-LE
    // feed PeriodOpenJob enumerates).
    let tenant_a = Uuid::now_v7();
    let tenant_b = Uuid::now_v7();
    for t in [tenant_a, tenant_b] {
        raw.execute_raw(pg(format!(
            "INSERT INTO bss.ledger_fiscal_calendar
               (tenant_id, legal_entity_id, fiscal_tz, granularity, fy_start_month)
             VALUES ('{t}','{t}','UTC','MONTH',1)"
        )))
        .await
        .unwrap();
    }

    // A provider that publishes two pairs.
    let provider = Arc::new(FakeProvider {
        id: "ecb".to_owned(),
        outcome: Outcome::Ok(vec![
            rate("EUR", "USD", 1_100_000),
            rate("GBP", "USD", 1_250_000),
        ]),
    });
    let repo = FxRepo::new(dbp.clone());
    let publisher = Arc::new(LedgerEventPublisher::noop());
    let job = RateSyncJob::new(dbp.clone(), provider, repo.clone(), publisher);

    let report = job.run().await.expect("fan-out run is Ok");
    assert!(report.fetched);
    assert_eq!(report.rates, 2);
    assert_eq!(report.tenants, 2);
    assert_eq!(report.failed_tenants, 0);

    // Each tenant's store now carries both pairs under the provider id.
    for t in [tenant_a, tenant_b] {
        let eur = repo.latest_rates(t, "EUR", "USD").await.unwrap();
        assert_eq!(eur.len(), 1, "EUR->USD upserted for {t}");
        assert_eq!(eur[0].provider, "ecb");
        assert_eq!(eur[0].rate_micro, 1_100_000);
        let gbp = repo.latest_rates(t, "GBP", "USD").await.unwrap();
        assert_eq!(gbp.len(), 1, "GBP->USD upserted for {t}");
        assert_eq!(gbp[0].rate_micro, 1_250_000);
    }
}
