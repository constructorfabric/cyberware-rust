//! The **shape** of the reads `OverlayRepo::world_for` issues, counted rather
//! than reasoned about.
//!
//! `overlay_facts` reads every published overlay of the tenant — the validation
//! genuinely needs the whole set, so that read is unbounded by design (§4.2) —
//! and then reads each one's lines. The defect this suite exists to keep out is
//! the *second* fan-out: one amounts query **per line**, which made the number of
//! round trips inside the publish transaction a function of the tenant's total
//! line count rather than of its overlay count.
//!
//! # Why this is a whole test binary of its own
//!
//! The claim is "how many statements", and nothing in the repository surface
//! reports that. `DBRunner` is sealed, so a counting runner cannot be written;
//! `sqlx` logs every statement it executes as a `tracing` event on the
//! `sqlx::query` target, and that is the only observation point. It has to be a
//! **global** subscriber — `sqlx_sqlite` runs each statement on the connection's
//! own worker thread, so a thread-local dispatcher installed by the test sees
//! nothing — and a global counter shared with the rest of a test binary would be
//! fed by whatever else runs in parallel. One test, one process, one counter.
//!
//! A probe over one overlay with one line could not tell the two shapes apart:
//! both issue one amounts query. So the seed is **three published overlays of
//! four lines each**, where the per-line shape issues twelve amounts reads and
//! the per-revision shape issues three, and the assertion names the number of
//! revisions rather than a literal.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bss_pricing::domain::audit::AuditStamp;
use bss_pricing::domain::instant::utc_ymd_hms;
use bss_pricing::domain::money::CurrencyCode;
use bss_pricing::domain::overlay::{
    Adjustment, AmountSet, Disclosure, LineKey, OverlayInterval, OverlayLine, ScopeClass,
    ScopeSelector, ScopeValue, TargetRef, TaxBasis,
};
use bss_pricing::domain::scope_key::PlanId;
use bss_pricing::infra::storage::migrations::Migrator;
use bss_pricing::infra::storage::repo::{NewOverlay, OverlayRepo};

use sea_orm_migration::MigratorTrait;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::AccessScope;
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};
use uuid::Uuid;

const TENANT: Uuid = Uuid::from_u128(0x1111_1111);
const CANDIDATE: Uuid = Uuid::from_u128(0xAAAA_0000);

/// Published overlays other than the candidate, and lines on each.
///
/// Both are read back into the assertions, so neither number is a literal the
/// probe compares against itself.
const PUBLISHED_OVERLAYS: usize = 3;
const LINES_PER_OVERLAY: usize = 4;

/// Counts the statements `sqlx` logs, split by the table each one names.
///
/// `pricing_price_overlay_line_amount` **contains** `pricing_price_overlay_line`,
/// so the amounts arm is tested first and the line arm is the `else`; matching
/// the shorter name first would count every amounts read twice.
struct StatementCounter {
    amount_statements: Arc<AtomicUsize>,
    line_statements: Arc<AtomicUsize>,
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
        if sql.0.contains("pricing_price_overlay_line_amount") {
            self.amount_statements.fetch_add(1, Ordering::Relaxed);
        } else if sql.0.contains("pricing_price_overlay_line") {
            self.line_statements.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}

/// Every field of the event, concatenated.
///
/// `sqlx` puts the statement in `summary` when it is short and in `db.statement`
/// when it is not, and renders `String` fields through either `record_str` or
/// `record_debug` depending on the `tracing` version. Reading all of them means
/// the probe does not rest on which.
struct StatementText(String);

impl Visit for StatementText {
    fn record_str(&mut self, _: &Field, value: &str) {
        self.0.push_str(value);
    }

    // `Visit`'s one **required** method renders through `Debug`, and a field the
    // count depends on may arrive that way. Both lints guard output a reader
    // sees; this buffer is never printed, only searched for a table name. The
    // formatting cannot fail into a `String`, and losing a field to a swallowed
    // error would surface as the "counter observed nothing" assertion rather
    // than as a wrong count.
    #[allow(clippy::use_debug, clippy::let_underscore_must_use)]
    fn record_debug(&mut self, _: &Field, value: &dyn std::fmt::Debug) {
        let _ = write!(self.0, "{value:?}");
    }
}

fn stamp() -> AuditStamp {
    AuditStamp {
        actor_principal_id: Uuid::from_u128(0x4444),
        recorded_at: utc_ymd_hms(2099, 1, 1, 0, 0, 0),
        correlation_id: Uuid::from_u128(0x5555),
    }
}

fn brand_scope() -> ScopeSelector {
    ScopeSelector::scoped(
        ScopeClass::Brand,
        ScopeValue::new("acme").expect("a non-blank value"),
    )
    .expect("brand is not global")
}

fn plan(n: u128) -> PlanId {
    PlanId::new(Uuid::from_u128(n))
}

fn overlay(id: Uuid, precedence: i32) -> NewOverlay {
    NewOverlay {
        price_overlay_id: id,
        tenant_id: TENANT,
        scope: brand_scope(),
        precedence,
        interval: OverlayInterval::default(),
        tax_basis: TaxBasis::DelegatedTariffs,
        disclosure: Disclosure::Restricted,
        target_ref: TargetRef {
            plans: (1..=u128::try_from(LINES_PER_OVERLAY).expect("small"))
                .map(plan)
                .collect(),
        },
    }
}

/// A `fixed` line in two currencies, both values derived from its own id.
///
/// Two currencies per line and a distinct pair per line is what makes a batched
/// read that groups its rows onto the wrong line visible: a single-currency seed
/// would still assemble plausibly if the grouping were dropped.
fn fixed_line(line_id: Uuid, plan_id: PlanId, base: i64) -> OverlayLine {
    OverlayLine {
        line_id,
        key: LineKey::for_plan(plan_id),
        adjustment: Adjustment::Fixed(AmountSet::new([
            (CurrencyCode::new("EUR").expect("a valid code"), base),
            (
                CurrencyCode::new("USD").expect("a valid code"),
                base.saturating_add(7),
            ),
        ])),
    }
}

/// `line_id`'s primary key is `(line_id, overlay_revision)` and is **not** scoped
/// by overlay, so every seeded line needs its own id even across overlays.
fn line_id(overlay_index: usize, line_index: usize) -> Uuid {
    let n = 0xCCCC_0000_u128
        + u128::try_from(overlay_index).expect("small") * 0x100
        + u128::try_from(line_index).expect("small");
    Uuid::from_u128(n)
}

#[tokio::test]
async fn the_overlay_world_reads_line_amounts_once_per_revision_and_not_once_per_line() {
    let amount_statements = Arc::new(AtomicUsize::new(0));
    let line_statements = Arc::new(AtomicUsize::new(0));
    tracing::subscriber::set_global_default(StatementCounter {
        amount_statements: Arc::clone(&amount_statements),
        line_statements: Arc::clone(&line_statements),
    })
    .expect("this binary installs the only subscriber");

    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("run migrator");
    let repo = OverlayRepo::new(DBProvider::<DbError>::new(db));
    let scope = AccessScope::allow_all();

    // The published set the world ranges over. Distinct precedences: the
    // precedence index is partial on `published` and unique within the class.
    let mut expected_amounts: Vec<(Uuid, i64)> = Vec::new();
    for n in 0..PUBLISHED_OVERLAYS {
        let id = Uuid::from_u128(0xBBBB_0000 + u128::try_from(n).expect("small"));
        let mut lines = Vec::new();
        for l in 0..LINES_PER_OVERLAY {
            let this = line_id(n, l);
            let base = i64::try_from(n.saturating_mul(100).saturating_add(l)).expect("small") + 1;
            expected_amounts.push((this, base));
            lines.push(fixed_line(
                this,
                plan(u128::try_from(l).expect("small") + 1),
                base,
            ));
        }
        repo.create(
            &scope,
            overlay(id, i32::try_from(n).expect("small") + 1),
            lines,
            stamp(),
        )
        .await
        .expect("the published overlay is created");
        repo.publish_revision(&scope, TENANT, id, 0, stamp())
            .await
            .expect("it publishes");
    }

    // The candidate, whose own overlay the world skips (D-107).
    repo.create(
        &scope,
        overlay(CANDIDATE, 50),
        vec![fixed_line(line_id(9, 0), plan(1), 999)],
        stamp(),
    )
    .await
    .expect("the candidate is created");
    let record = repo
        .load(&scope, TENANT, CANDIDATE, 0)
        .await
        .expect("the read succeeds")
        .expect("revision 0 exists");

    amount_statements.store(0, Ordering::Relaxed);
    line_statements.store(0, Ordering::Relaxed);
    let world = repo
        .world_for(&scope, TENANT, &record)
        .await
        .expect("the world assembles");
    let amounts_read = amount_statements.load(Ordering::Relaxed);
    let lines_read = line_statements.load(Ordering::Relaxed);

    // The counter is armed: if the mechanism observed nothing, this is the
    // assertion that says so rather than the count silently agreeing with a
    // batched shape it never measured.
    assert!(
        lines_read > 0,
        "the statement counter observed no line read at all, so it is measuring nothing"
    );
    assert_eq!(
        world.interval_holders.len(),
        PUBLISHED_OVERLAYS * LINES_PER_OVERLAY,
        "the world must still see every published line; a batching change that read \
         fewer rows would otherwise satisfy the count assertion below"
    );
    assert_eq!(
        amounts_read,
        PUBLISHED_OVERLAYS,
        "one amounts read per published revision. Per line it is {} - the shape this \
         suite exists to refuse",
        PUBLISHED_OVERLAYS * LINES_PER_OVERLAY
    );

    // And the batching groups its rows onto the right line. Read back through
    // `load`, which shares the batched path, so a grouping that handed one line
    // another's currencies reddens here rather than in a caller.
    for n in 0..PUBLISHED_OVERLAYS {
        let id = Uuid::from_u128(0xBBBB_0000 + u128::try_from(n).expect("small"));
        let stored = repo
            .load(&scope, TENANT, id, 0)
            .await
            .expect("the read succeeds")
            .expect("revision 0 exists");
        assert_eq!(stored.lines.len(), LINES_PER_OVERLAY);
        for line in &stored.lines {
            let (_, base) = expected_amounts
                .iter()
                .find(|(seeded, _)| *seeded == line.line_id)
                .expect("every read line was seeded");
            let amounts = line
                .adjustment
                .amounts()
                .expect("a fixed line carries its amount set");
            let mut values: Vec<i64> = amounts.iter().map(|(_, value)| value).collect();
            values.sort_unstable();
            assert_eq!(
                values,
                vec![*base, base.saturating_add(7)],
                "line {} must carry its own two amounts and nobody else's",
                line.line_id
            );
        }
    }
}
