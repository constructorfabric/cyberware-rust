//! The admission path's emission sites (T16).

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::doc_markdown)]

mod common;

use std::io;
use std::sync::{Arc, Mutex, OnceLock};

use opentelemetry::metrics::MeterProvider;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::metrics::{
    InMemoryMetricExporter, InMemoryMetricExporterBuilder, PeriodicReader, SdkMeterProvider,
    Temporality,
};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::macros::datetime;
use toolkit_db::{DBProvider, DbError, DbTx};
use toolkit_gts::gts_id;
use tracing_subscriber::fmt::MakeWriter;
use uuid::Uuid;

use common::{
    PausePoint, PausingStores, TestDir, allow_all, stores, test_db, test_db_file, worker_settings,
};
use types_registry::config::{MetricsConfig, TypesRegistryConfig};
use types_registry::domain::admission::AdmissionFailureReason;
use types_registry::domain::admission::acceptance::{AcceptanceContext, AcceptanceError, accept};
use types_registry::domain::admission::worker::{
    ItemFailure, OperationOutcome, Tuning, WorkerError, reason_label, run_operation,
};
use types_registry::domain::admission::{Candidate, OperationDispatch, SubmitRequest};
use types_registry::domain::enums as domain_enums;
use types_registry::domain::enums::OperationItemStatus;
use types_registry::domain::policy::RegistrationPolicy;
use types_registry::domain::ports::Stores;
use types_registry::domain::ports::metrics::{AdmissionMetrics, RefusalStage};
use types_registry::infra::metrics::{AdmissionMetricsMeter, SCOPE};

const NOW: OffsetDateTime = datetime!(2026-08-21 09:15:30 UTC);
const LATER: OffsetDateTime = datetime!(2026-08-21 10:20:40 UTC);

const SUBJECT: &str = gts_id!("cf.core.obsv.subject.v1~");
const REFERRER: &str = gts_id!("cf.core.obsv.referrer.v1~");
const MIDDLE: &str = gts_id!("cf.core.obsv.middle.v1~");
const ABSENT: &str = gts_id!("cf.core.obsv.absent.v1~");

type Provider = Arc<DBProvider<DbError>>;

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

static LOG: Mutex<Vec<u8>> = Mutex::new(Vec::new());

struct LogWriter;

impl io::Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        LOG.lock().expect("log buffer").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl MakeWriter<'_> for LogWriter {
    type Writer = Self;

    fn make_writer(&self) -> Self::Writer {
        Self
    }
}

fn recorder() -> &'static (SdkMeterProvider, InMemoryMetricExporter) {
    static RECORDER: OnceLock<(SdkMeterProvider, InMemoryMetricExporter)> = OnceLock::new();
    RECORDER.get_or_init(|| {
        let exporter = InMemoryMetricExporterBuilder::new()
            .with_temporality(Temporality::Delta)
            .build();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        opentelemetry::global::set_meter_provider(provider.clone());

        let subscriber = tracing_subscriber::fmt()
            .with_env_filter("types_registry=debug")
            .with_writer(LogWriter)
            .with_ansi(false)
            .finish();
        tracing::subscriber::set_global_default(subscriber)
            .expect("this binary installs exactly one subscriber");

        (provider, exporter)
    })
}

fn captured_log() -> String {
    String::from_utf8_lossy(&LOG.lock().expect("log buffer")).into_owned()
}

fn lines_mentioning(needle: &str) -> Vec<String> {
    captured_log()
        .lines()
        .filter(|line| line.contains(needle))
        .map(str::to_owned)
        .collect()
}

fn metrics() -> &'static Arc<dyn AdmissionMetrics> {
    static METRICS: OnceLock<Arc<dyn AdmissionMetrics>> = OnceLock::new();
    METRICS.get_or_init(|| {
        let (provider, _) = recorder();
        Arc::new(AdmissionMetricsMeter::new(
            &provider.meter(SCOPE),
            &MetricsConfig::default().effective_prefix("types-registry"),
        ))
    })
}

fn counter_sum_where(name: &str, labels: &[(&str, &str)]) -> u64 {
    let metrics = recorder().1.get_finished_metrics().unwrap();
    let mut total = 0;
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data()
                {
                    total += sum
                        .data_points()
                        .filter(|dp| {
                            labels.iter().all(|(key, value)| {
                                dp.attributes().any(|kv| {
                                    kv.key.as_str() == *key && kv.value.as_str() == *value
                                })
                            })
                        })
                        .map(opentelemetry_sdk::metrics::data::SumDataPoint::value)
                        .sum::<u64>();
                }
            }
        }
    }
    total
}

fn histogram_count(name: &str) -> u64 {
    let metrics = recorder().1.get_finished_metrics().unwrap();
    let mut total = 0;
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::F64(MetricData::Histogram(h)) = metric.data()
                {
                    total += h
                        .data_points()
                        .map(opentelemetry_sdk::metrics::data::HistogramDataPoint::count)
                        .sum::<u64>();
                }
            }
        }
    }
    total
}

fn histogram_sum(name: &str) -> f64 {
    let metrics = recorder().1.get_finished_metrics().unwrap();
    let mut total = 0.0;
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::F64(MetricData::Histogram(h)) = metric.data()
                {
                    total += h
                        .data_points()
                        .map(opentelemetry_sdk::metrics::data::HistogramDataPoint::sum)
                        .sum::<f64>();
                }
            }
        }
    }
    total
}

fn reset_metrics() {
    let (provider, exporter) = recorder();
    provider.force_flush().expect("flush");
    exporter.reset();
}

fn flush() {
    recorder().0.force_flush().expect("flush");
}

struct NoDispatch;

#[async_trait::async_trait]
impl OperationDispatch for NoDispatch {
    async fn enqueue(&self, _tx: &DbTx<'_>, _operation_id: Uuid) -> anyhow::Result<()> {
        Ok(())
    }
}

fn worker(db: &Provider) -> DBProvider<WorkerError> {
    DBProvider::new(db.db())
}

fn subject_schema(property: &str) -> Value {
    json!({
        "$id": format!("gts://{SUBJECT}"),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": { property: { "type": "string" } },
    })
}

fn referencing_schema(marker: &str) -> Value {
    json!({
        "$id": format!("gts://{REFERRER}"),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {
            "subject": { "$ref": format!("gts://{SUBJECT}") },
            marker: { "type": "string" },
        },
    })
}

fn middle_schema() -> Value {
    json!({
        "$id": format!("gts://{MIDDLE}"),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": { "subject": { "$ref": format!("gts://{SUBJECT}") } },
    })
}

/// A referrer that reaches `SUBJECT` transitively through `MIDDLE`, leaving the
/// revision-vector guard to detect its movement.
fn chained_schema(marker: &str) -> Value {
    json!({
        "$id": format!("gts://{REFERRER}"),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {
            "middle": { "$ref": format!("gts://{MIDDLE}") },
            marker: { "type": "string" },
        },
    })
}

fn absent_schema() -> Value {
    json!({
        "$id": format!("gts://{ABSENT}"),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
    })
}

async fn submit(
    db: &Provider,
    key: &str,
    candidates: Vec<Candidate>,
) -> Result<Uuid, AcceptanceError> {
    submit_via(db, key, Arc::new(NoDispatch), candidates).await
}

async fn submit_via(
    db: &Provider,
    key: &str,
    dispatch: Arc<dyn OperationDispatch>,
    candidates: Vec<Candidate>,
) -> Result<Uuid, AcceptanceError> {
    let provider: DBProvider<AcceptanceError> = DBProvider::new(db.db());
    let policy = RegistrationPolicy::default();
    let config = TypesRegistryConfig::default();
    accept(
        &stores(),
        &provider,
        &allow_all(),
        &AcceptanceContext {
            policy: &policy,
            config: &config,
            metrics: metrics(),
        },
        &dispatch,
        &SubmitRequest {
            idempotency_key: key.to_owned(),
            kind: domain_enums::OperationKind::Registration,
            dry_run: false,
            candidates,
        },
        NOW,
    )
    .await
    .map(|accepted| accepted.operation_id)
}

fn candidate(gts_id: &str, content: Value, expected_resource_version: Option<i64>) -> Candidate {
    Candidate {
        gts_id: gts_id.to_owned(),
        content: Some(content),
        expected_resource_version,
        force: false,
    }
}

async fn admit(
    db: &Provider,
    key: &str,
    gts_id: &str,
    content: Value,
    expected_resource_version: Option<i64>,
) -> OperationOutcome {
    let operation_id = submit(
        db,
        key,
        vec![candidate(gts_id, content, expected_resource_version)],
    )
    .await
    .expect("acceptance");
    run_operation(
        &stores(),
        &worker(db),
        &allow_all(),
        Tuning {
            limits: &common::limits(),
            worker: &worker_settings(),
            metrics: metrics(),
        },
        operation_id,
        LATER,
    )
    .await
    .expect("the worker must not fail on infrastructure")
}

#[tokio::test]
async fn a_successful_admission_counts_one_succeeded_candidate_and_one_pass_duration() {
    let _serial = SERIAL.lock().await;
    recorder();
    let db = test_db().await;
    reset_metrics();

    let outcome = admit(&db, "k-ok", SUBJECT, subject_schema("name"), None).await;
    flush();

    assert_eq!(outcome.items[0].status, OperationItemStatus::Succeeded);
    assert_eq!(
        counter_sum_where(
            "types_registry_candidates_total",
            &[("status", "succeeded")],
        ),
        1,
    );
    assert_eq!(
        histogram_count("types_registry_operation_duration_seconds"),
        1,
        "one pass, one observation",
    );
}

#[tokio::test]
async fn a_redundant_resubmission_counts_an_unchanged_candidate() {
    let _serial = SERIAL.lock().await;
    recorder();
    let db = test_db().await;
    admit(&db, "k-first", SUBJECT, subject_schema("name"), None).await;
    reset_metrics();

    let outcome = admit(&db, "k-again", SUBJECT, subject_schema("name"), Some(1)).await;
    flush();

    assert_eq!(outcome.items[0].status, OperationItemStatus::Unchanged);
    assert_eq!(
        counter_sum_where("types_registry_unchanged_probes_total", &[("hit", "true")]),
        1
    );
    assert_eq!(
        counter_sum_where("types_registry_unchanged_probes_total", &[("hit", "false")]),
        0
    );
    assert_eq!(
        counter_sum_where(
            "types_registry_candidates_total",
            &[("status", "unchanged")],
        ),
        1,
    );
    assert_eq!(
        counter_sum_where(
            "types_registry_candidates_total",
            &[("status", "succeeded")],
        ),
        0,
        "an unchanged re-submission is not a success",
    );
}

#[tokio::test]
async fn an_admission_refusal_counts_a_failed_candidate_and_its_reason() {
    let _serial = SERIAL.lock().await;
    recorder();
    let db = test_db().await;
    reset_metrics();

    let outcome = admit(&db, "k-absent", ABSENT, absent_schema(), Some(1)).await;
    flush();

    assert_eq!(outcome.items[0].status, OperationItemStatus::Failed);
    assert_eq!(
        counter_sum_where("types_registry_candidates_total", &[("status", "failed")]),
        1,
    );
    assert_eq!(
        counter_sum_where(
            "types_registry_refusals_total",
            &[("stage", "admission"), ("reason", "precondition_failed")],
        ),
        1,
    );
    assert_eq!(
        counter_sum_where("types_registry_refusals_total", &[("stage", "acceptance")],),
        0,
        "the request was accepted; only the candidate was refused",
    );
}

#[tokio::test]
async fn an_acceptance_refusal_counts_under_its_own_stage_and_reason() {
    let _serial = SERIAL.lock().await;
    recorder();
    let db = test_db().await;
    reset_metrics();

    let refused = submit(&db, "k-empty", Vec::new()).await;
    flush();

    assert!(matches!(refused, Err(AcceptanceError::EmptyBatch)));
    assert_eq!(
        counter_sum_where(
            "types_registry_refusals_total",
            &[("stage", "acceptance"), ("reason", "empty_batch")],
        ),
        1,
    );
    assert_eq!(
        counter_sum_where("types_registry_candidates_total", &[]),
        0,
        "nothing was admitted, so no candidate reached a terminal status",
    );
}

#[tokio::test]
async fn two_acceptance_refusals_are_told_apart_by_their_reason() {
    let _serial = SERIAL.lock().await;
    recorder();
    let db = test_db().await;
    reset_metrics();

    submit(
        &db,
        "k-dup",
        vec![
            candidate(SUBJECT, subject_schema("name"), None),
            candidate(SUBJECT, subject_schema("name"), None),
        ],
    )
    .await
    .expect_err("a duplicate candidate is refused");
    submit(
        &db,
        "k-zero",
        vec![candidate(SUBJECT, subject_schema("name"), Some(0))],
    )
    .await
    .expect_err("expected_resource_version 0 is refused");
    flush();

    assert_eq!(
        counter_sum_where(
            "types_registry_refusals_total",
            &[("reason", "duplicate_candidate")],
        ),
        1,
    );
    assert_eq!(
        counter_sum_where(
            "types_registry_refusals_total",
            &[("reason", "zero_precondition")],
        ),
        1,
    );
}

#[tokio::test]
async fn an_unknown_stored_failure_reason_counts_under_other_and_creates_no_new_series() {
    let _serial = SERIAL.lock().await;
    recorder();
    reset_metrics();

    let failure = ItemFailure::from_payload(
        r#"{"reason":"future_refusal_code","message":"read back off a stored row"}"#,
    );
    assert!(
        matches!(failure.reason, AdmissionFailureReason::Unknown(_)),
        "unknown stored codes must be preserved"
    );
    metrics().refused(RefusalStage::Admission, reason_label(&failure.reason));
    flush();

    assert_eq!(
        counter_sum_where(
            "types_registry_refusals_total",
            &[("stage", "admission"), ("reason", "other")],
        ),
        1,
    );
    assert_eq!(
        counter_sum_where(
            "types_registry_refusals_total",
            &[("reason", "future_refusal_code")],
        ),
        0,
        "an unknown reason must count under `other`, not as its own series",
    );
}

#[tokio::test]
async fn an_infrastructure_failure_is_counted_but_not_warned_as_a_refusal() {
    struct FailingDispatch;
    #[async_trait::async_trait]
    impl OperationDispatch for FailingDispatch {
        async fn enqueue(&self, _tx: &DbTx<'_>, _operation_id: Uuid) -> anyhow::Result<()> {
            Err(anyhow::anyhow!("observability-dispatch-outage"))
        }
    }

    let _serial = SERIAL.lock().await;
    recorder();
    let db = test_db().await;
    reset_metrics();

    // Use a unique identifier so this refusal is filterable in the shared log.
    let client = gts_id!("cf.core.obsv.warnclient.v1~");
    let refused = submit(
        &db,
        "k-warn-client",
        vec![candidate(client, subject_schema("name"), Some(0))],
    )
    .await;
    assert!(matches!(
        refused,
        Err(AcceptanceError::ZeroPrecondition { .. })
    ));

    let infra = gts_id!("cf.core.obsv.warninfra.v1~");
    let content = json!({
        "$id": format!("gts://{infra}"),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
    });
    let failed = submit_via(
        &db,
        "k-warn-infra",
        Arc::new(FailingDispatch),
        vec![candidate(infra, content, None)],
    )
    .await;
    assert!(
        matches!(failed, Err(AcceptanceError::Dispatch(_))),
        "the dispatch failure is the arm under test, got {failed:?}"
    );
    flush();

    assert!(
        lines_mentioning(client)
            .iter()
            .any(|line| line.contains("types_registry refused a submission")),
        "a client refusal is warned; captured:\n{}",
        captured_log()
    );
    // Absence proves the dispatch error was not logged by `accept`.
    assert!(
        !captured_log().contains("observability-dispatch-outage"),
        "an infrastructure arm must not be logged as a refusal; captured:\n{}",
        captured_log()
    );

    assert_eq!(
        counter_sum_where(
            "types_registry_refusals_total",
            &[("stage", "acceptance"), ("reason", "zero_precondition")],
        ),
        1,
        "the client refusal is counted",
    );
    assert_eq!(
        counter_sum_where(
            "types_registry_refusals_total",
            &[("stage", "acceptance"), ("reason", "dispatch_failure")],
        ),
        1,
        "the infrastructure fault is counted too; the counter has no `if`",
    );
}

#[tokio::test]
async fn a_revision_observes_the_activation_write_set_it_rewrote() {
    let _serial = SERIAL.lock().await;
    recorder();
    let db = test_db().await;
    admit(&db, "k-subject", SUBJECT, subject_schema("name"), None).await;
    admit(
        &db,
        "k-referrer",
        REFERRER,
        referencing_schema("note"),
        None,
    )
    .await;
    reset_metrics();

    let outcome = admit(
        &db,
        "k-subject-2",
        SUBJECT,
        subject_schema("label"),
        Some(1),
    )
    .await;
    flush();

    assert_eq!(outcome.items[0].status, OperationItemStatus::Succeeded);
    assert_eq!(
        histogram_count("types_registry_activation_write_set"),
        1,
        "one revision, one observation",
    );
    let refreshed = histogram_sum("types_registry_activation_write_set");
    assert!(
        (refreshed - 1.0).abs() < f64::EPSILON,
        "exactly the one dependent was rewritten, got {refreshed}",
    );
}

#[tokio::test]
async fn a_creation_observes_no_activation_write_set() {
    let _serial = SERIAL.lock().await;
    recorder();
    let db = test_db().await;
    reset_metrics();

    let outcome = admit(&db, "k-lonely", SUBJECT, subject_schema("name"), None).await;
    flush();

    assert_eq!(outcome.items[0].status, OperationItemStatus::Succeeded);
    assert_eq!(
        histogram_count("types_registry_activation_write_set"),
        0,
        "a creation has no dependents to refresh, so it observes no write set",
    );
    assert_eq!(
        counter_sum_where(
            "types_registry_candidates_total",
            &[("status", "succeeded")],
        ),
        1,
        "the control: the pass did run, so the zero above is scope and not silence",
    );
}

#[tokio::test]
async fn a_revalidation_retry_is_counted_by_its_drift_shape() {
    let _serial = SERIAL.lock().await;
    recorder();
    let dir = TestDir::new("types-registry-obsv-retry");
    let db = test_db_file(&dir.path().join("registry.db")).await;
    admit(&db, "k-subject", SUBJECT, subject_schema("name"), None).await;
    admit(&db, "k-middle", MIDDLE, middle_schema(), None).await;
    admit(&db, "k-referrer", REFERRER, chained_schema("note"), None).await;

    let operation_id = submit(
        &db,
        "k-referrer-2",
        vec![candidate(REFERRER, chained_schema("tag"), Some(1))],
    )
    .await
    .expect("acceptance");
    reset_metrics();

    // Held after evaluation and immediately before the commit's first statement — see
    // `revalidation_test.rs`.
    let (paused, reached, resume) = PausingStores::new(PausePoint::BeforeEntityWriteOrderClaim);
    let ports: Arc<dyn Stores> = paused;
    let provider = worker(&db);
    let pass = tokio::spawn(async move {
        run_operation(
            &ports,
            &provider,
            &allow_all(),
            Tuning {
                limits: &common::limits(),
                worker: &worker_settings(),
                metrics: metrics(),
            },
            operation_id,
            LATER,
        )
        .await
    });

    reached.await.expect("the pass must reach the commit");
    let mutating = Arc::clone(&db);
    admit(
        &mutating,
        "k-subject-2",
        SUBJECT,
        subject_schema("label"),
        Some(1),
    )
    .await;
    resume.send(()).expect("the pass must still be waiting");
    let outcome = pass
        .await
        .expect("the pass task must not panic")
        .expect("the worker must not fail on infrastructure");
    flush();

    assert_eq!(
        outcome.items[0].status,
        OperationItemStatus::Succeeded,
        "the retry must succeed, got {:?}",
        outcome.items[0],
    );
    assert_eq!(
        counter_sum_where("types_registry_revalidations_total", &[("drift", "moved")]),
        1,
        "one rollback, counted under the drift that caused it",
    );
    assert_eq!(
        counter_sum_where("types_registry_unchanged_probes_total", &[("hit", "false")]),
        2,
        "one miss for each operation, with no extra probe on revalidation"
    );
}

#[tokio::test]
async fn the_real_pass_wraps_its_work_in_an_operation_span_and_a_unit_span() {
    let _serial = SERIAL.lock().await;
    recorder();
    let db = test_db().await;
    let unique = gts_id!("cf.core.obsv.spanned.v1~");
    let content = json!({
        "$id": format!("gts://{unique}"),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
    });

    let outcome = admit(&db, "k-span", unique, content, None).await;
    let operation_id = outcome.operation_id;

    let lines = lines_mentioning(unique);
    assert!(
        !lines.is_empty(),
        "the admission must log something naming the candidate; captured:\n{}",
        captured_log()
    );
    let admitted = lines
        .iter()
        .find(|line| line.contains("candidate admitted"))
        .unwrap_or_else(|| panic!("no admission line among:\n{}", lines.join("\n")));

    assert!(
        admitted.contains("types_registry.admission.operation"),
        "the operation span must be on the line: {admitted}"
    );
    assert!(
        admitted.contains("types_registry.admission.unit"),
        "the unit span must be on the line: {admitted}"
    );
    assert!(
        admitted.contains(&operation_id.to_string()),
        "operation_id must be on the line: {admitted}"
    );
    assert!(
        admitted.contains(r#"kind="registration""#),
        "the operation kind must be on the line: {admitted}"
    );
    assert!(
        admitted.contains("dry_run=false"),
        "the dry-run mode must be on the line: {admitted}"
    );
}

#[tokio::test]
async fn a_redelivered_pass_still_carries_the_operation_facts() {
    let _serial = SERIAL.lock().await;
    recorder();
    let db = test_db().await;
    let unique = gts_id!("cf.core.obsv.redelivered.v1~");
    let content = json!({
        "$id": format!("gts://{unique}"),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
    });
    let operation_id = submit(&db, "k-redeliver", vec![candidate(unique, content, None)])
        .await
        .expect("acceptance");

    run_operation(
        &stores(),
        &worker(&db),
        &allow_all(),
        Tuning {
            limits: &common::limits(),
            worker: &worker_settings(),
            metrics: metrics(),
        },
        operation_id,
        LATER,
    )
    .await
    .expect("the worker must not fail on infrastructure");
    let first_pass_lines = lines_mentioning(&operation_id.to_string()).len();

    // Keep only the redelivered pass's counts.
    reset_metrics();

    run_operation(
        &stores(),
        &worker(&db),
        &allow_all(),
        Tuning {
            limits: &common::limits(),
            worker: &worker_settings(),
            metrics: metrics(),
        },
        operation_id,
        LATER,
    )
    .await
    .expect("the worker must not fail on infrastructure");
    flush();

    // Inspect only lines appended by the second pass.
    let all_lines = lines_mentioning(&operation_id.to_string());
    let redelivered_lines = &all_lines[first_pass_lines..];
    assert!(
        !redelivered_lines.is_empty(),
        "the redelivered pass must emit its own log line; captured:\n{}",
        all_lines.join("\n")
    );
    assert!(
        redelivered_lines
            .iter()
            .any(|line| line.contains(r#"kind="registration""#)),
        "the operation facts must be recorded on the redelivered pass's span; \
         captured:\n{}",
        redelivered_lines.join("\n")
    );

    // Redelivery records duration but does not terminalize the candidate again.
    assert_eq!(
        counter_sum_where("types_registry_candidates_total", &[]),
        0,
        "a redelivered pass must not re-count the candidate it reported",
    );
    assert_eq!(
        histogram_count("types_registry_operation_duration_seconds"),
        1,
        "the redelivered pass still observes the one duration it spent",
    );
}
