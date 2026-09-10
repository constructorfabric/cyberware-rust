//! The bulk import's three surfaces, over HTTP
//! (`design/12-operator-efficiency.md` §5, `inst-bi-api`, `inst-bi-return`,
//! `inst-bk-idem`, `inst-bs-abort`; D-293).
//!
//! Every case asserts what the **run** holds as well as what the response said:
//! `inst-bi-return` makes the response an operation ref and the `GET` the place
//! the answer lives, so a suite that only read responses would be testing the
//! smaller half of the contract.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;
mod rest_support;

use axum::http::StatusCode;
use bss_pricing::api::rest::bulk_imports::BULK_IMPORTS;
use bss_pricing::authz::{actions, labels};
use bss_pricing::domain::bulk::{BulkKind, BulkState};
use bss_pricing::infra::storage::entity::price;
use bss_pricing::infra::storage::repo::bulk_repo;
use rest_support::{
    Harness, body_json, bulk_operation_row, problem_code, seed_current_plan, with_headers,
};
use sea_orm::ActiveValue::Set;
use sea_orm::EntityTrait;
use time::OffsetDateTime;
use toolkit_db::secure::{SecureEntityExt, SecureInsertExt};
use uuid::Uuid;

fn keyed(key: &str) -> Vec<(&str, &str)> {
    vec![("idempotency-key", key)]
}

fn import_path(operation_id: &str) -> String {
    format!("{BULK_IMPORTS}/{operation_id}")
}

fn abort_path(operation_id: &str) -> String {
    format!("{BULK_IMPORTS}/{operation_id}/abort")
}

/// One row on a named region of a named plan.
fn row(plan_id: Uuid, region: &str, amount: i64) -> serde_json::Value {
    serde_json::json!({
        "plan_id": plan_id,
        "scope_key": {
            "currency": "USD",
            "region": region,
            "phase": rest_support::seeded_phase().get().to_string(),
            "price_eligibility": "all_subscriptions",
            "charge_kind": "recurring",
            "cohort": serde_json::Value::Null
        },
        "content": {
            "model_kind": "flat",
            "amount_minor": amount,
            "tax_inclusive": false
        }
    })
}

fn batch(rows: &[serde_json::Value]) -> serde_json::Value {
    serde_json::json!({ "rows": rows })
}

#[tokio::test]
async fn a_batch_of_new_keys_is_accepted_and_the_run_reports_what_landed() {
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            BULK_IMPORTS,
            Some(batch(&[row(plan, "eu", 1_500), row(plan, "us", 2_500)])),
            &keyed("bulk-1"),
        ))
        .await;

    assert_eq!(response.status(), StatusCode::ACCEPTED, "inst-bi-return");
    let body = body_json(response).await;
    assert_eq!(body["state"], serde_json::json!("completed"));
    let operation_id = body["operation_id"].as_str().expect("the ref").to_owned();

    // The answer lives at the GET, for the first caller and every replay alike.
    let read = harness
        .allowed()
        .send(with_headers("GET", &import_path(&operation_id), None, &[]))
        .await;
    assert_eq!(read.status(), StatusCode::OK);
    let run = body_json(read).await;
    assert_eq!(
        run["report"]["committed"]
            .as_array()
            .expect("an array")
            .len(),
        2,
        "both rows landed and the run says so: {run}"
    );
}

#[tokio::test]
async fn a_duplicate_inside_the_batch_blocks_it_and_commits_nothing() {
    // Phase 1's all-or-nothing posture, over HTTP. The refusal is a 400 carrying
    // its code, not a 422: Foundation section 3.3 gives the platform no 422
    // category, and the per-row report is at the GET.
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            BULK_IMPORTS,
            Some(batch(&[row(plan, "eu", 1_500), row(plan, "eu", 2_500)])),
            &keyed("bulk-2"),
        ))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        problem_code(response).await,
        "BULK_VALIDATION_FAILED",
        "the batch-level code names what happened"
    );
    // The second half of this case's own name, and it was unmeasured: a Phase-1
    // refusal that had already written the first of the two rows answers the same
    // 400 with the same code. The status says the batch was refused; only the
    // store says nothing was committed.
    assert!(
        rest_support::price_rows(&harness, plan).await.is_empty(),
        "the refused batch committed a row"
    );
}

/// **The positive control, and it is armed on the *same* batch** (Z11-5).
///
/// This case used to replay a **different** batch — `row(plan, "us", 9_900)`
/// against a first call's `row(plan, "eu", 1_500)` — and assert `202` with the
/// first run's report. That is not `inst-bk-idem`: it pinned the inversion Z11-5
/// found, because a caller whose corrected batch is answered `202` over a stale
/// report has been told a resubmit succeeded that imported nothing. The replay
/// property is about **one request** arriving twice, so the second body is the
/// first one byte for byte, and the different-batch case is the sibling below.
#[tokio::test]
async fn a_replayed_key_answers_the_run_it_opened_and_imports_nothing_twice() {
    // `inst-bk-idem`. The run's own unique `(tenant, client_key)` is the record,
    // not a dedup row with a TTL — which is what lets the replay work "during and
    // after the lock window".
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;

    let first = harness
        .allowed()
        .send(with_headers(
            "POST",
            BULK_IMPORTS,
            Some(batch(&[row(plan, "eu", 1_500)])),
            &keyed("bulk-3"),
        ))
        .await;
    let first_id = body_json(first).await["operation_id"].clone();

    let replay = harness
        .allowed()
        .send(with_headers(
            "POST",
            BULK_IMPORTS,
            Some(batch(&[row(plan, "eu", 1_500)])),
            &keyed("bulk-3"),
        ))
        .await;

    assert_eq!(replay.status(), StatusCode::ACCEPTED);
    let replayed = body_json(replay).await;
    assert_eq!(
        replayed["operation_id"], first_id,
        "a replay answers the run the key opened"
    );
    assert_eq!(
        replayed["report"]["committed"]
            .as_array()
            .expect("an array")
            .len(),
        1,
        "and the second body imported nothing: {replayed}"
    );
}

/// **The RED this case is about** (Z11-5).
///
/// The replay was `find_by_client_key` and nothing else: the body was never
/// compared with what the key first carried. So an operator who corrected a batch
/// and resubmitted it under the same key was answered `202`, handed the **first**
/// batch's report, and imported nothing — with no member in `BulkImportView` that
/// could reveal the substitution. That is the same inversion D-295 closed on the
/// state axis and D-307 on the kind axis, on the third one, and the crate's own
/// interactive gate has carried the guard from the start
/// (`idempotency_repo::claim` → `IDEMPOTENCY_PAYLOAD_MISMATCH`).
///
/// Armed on a **genuinely different** payload: the second batch prices a region
/// the first never named. Replaying the same payload proves the opposite property
/// and is the case immediately above, which is this one's positive control.
#[tokio::test]
async fn a_different_batch_under_a_spent_key_is_refused_rather_than_answered_202() {
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;

    let first = harness
        .allowed()
        .send(with_headers(
            "POST",
            BULK_IMPORTS,
            Some(batch(&[row(plan, "eu", 1_500)])),
            &keyed("bulk-payload-guard"),
        ))
        .await;
    assert_eq!(first.status(), StatusCode::ACCEPTED);
    let first = body_json(first).await;
    let first_id = first["operation_id"].as_str().expect("the ref").to_owned();

    // A different batch: another region, another amount, under the spent key.
    let refused = harness
        .allowed()
        .send(with_headers(
            "POST",
            BULK_IMPORTS,
            Some(batch(&[row(plan, "us", 9_900)])),
            &keyed("bulk-payload-guard"),
        ))
        .await;

    assert_eq!(
        refused.status(),
        StatusCode::CONFLICT,
        "a batch the key never carried must not be answered as though it had been imported"
    );
    assert_eq!(
        problem_code(refused).await,
        "IDEMPOTENCY_PAYLOAD_MISMATCH",
        "and it is refused under the code the interactive gate already uses for it"
    );

    // Nothing of the second batch reached the store, and the run still holds the
    // first batch's answer: the refusal is not a partial import.
    let read = harness
        .allowed()
        .send(with_headers("GET", &import_path(&first_id), None, &[]))
        .await;
    assert_eq!(read.status(), StatusCode::OK);
    let run = body_json(read).await;
    assert_eq!(
        run["report"]["committed"]
            .as_array()
            .expect("an array")
            .len(),
        1,
        "the run is still the first batch's, unmoved by the refusal: {run}"
    );
    let landed = rest_support::price_rows(&harness, plan).await;
    assert_eq!(
        landed.len(),
        1,
        "and the second batch's row was never imported: {landed:?}"
    );
}

/// **A run that predates the digest replays as it always did** (Z11-5).
///
/// The refusal above must not reach the runs `pricing_bulk_operation` backfilled: their
/// bodies are stored nowhere, so "the digests differ" would be a claim the store
/// cannot support, and refusing them would spend the harm on the caller who did
/// nothing wrong. A `!=` written without the emptiness test would refuse every one
/// of them, and no case armed on a *live* run could see it — this one opens the run
/// through the repository with the empty digest the `ALTER` left, which is the only
/// way that row shape now exists (the column is frozen against `UPDATE` since
/// `pricing_bulk_operation`).
#[tokio::test]
async fn a_run_opened_before_the_digest_existed_still_replays() {
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;

    let conn = harness.db.conn().expect("conn");
    let opened = bulk_repo::open(
        &conn,
        &harness.scope(),
        bss_pricing::infra::storage::repo::NewBulkOperation {
            operation_id: Uuid::now_v7(),
            tenant_id: harness.tenant,
            kind: BulkKind::Import,
            client_key: "bulk-pre-digest".to_owned(),
            // The backfilled value, and the only one no writer produces.
            request_hash: Vec::new(),
            report: serde_json::json!({ "rows": [] }),
            submitted_by: Uuid::from_u128(0xac_12),
            submitted_at: OffsetDateTime::now_utc(),
        },
    )
    .await
    .expect("open a run the way the pre-Z11-5 writer did");

    let replay = harness
        .allowed()
        .send(with_headers(
            "POST",
            BULK_IMPORTS,
            Some(batch(&[row(plan, "eu", 1_500)])),
            &keyed("bulk-pre-digest"),
        ))
        .await;

    assert_eq!(
        replay.status(),
        StatusCode::ACCEPTED,
        "a run whose payload nobody recorded cannot be told its payload differs"
    );
    let replayed = body_json(replay).await;
    assert_eq!(
        replayed["operation_id"],
        serde_json::json!(opened.operation_id),
        "and it is answered the run its key opened: {replayed}"
    );
}

#[tokio::test]
async fn a_submit_without_an_idempotency_key_is_refused() {
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            BULK_IMPORTS,
            Some(batch(&[row(plan, "eu", 1_500)])),
            &[],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn an_absent_run_reads_as_absent() {
    let harness = Harness::new().await;
    let response = harness
        .allowed()
        .send(with_headers(
            "GET",
            &import_path(&Uuid::now_v7().to_string()),
            None,
            &[],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn aborting_a_finished_run_is_refused_by_the_state_machine() {
    // Abort is an edge out of `committing` and nothing else. A run that already
    // completed has no lock to clear and no work to stop, so the refusal comes
    // from the trigger rather than from a check written here.
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;

    let submitted = harness
        .allowed()
        .send(with_headers(
            "POST",
            BULK_IMPORTS,
            Some(batch(&[row(plan, "eu", 1_500)])),
            &keyed("bulk-4"),
        ))
        .await;
    let operation_id = body_json(submitted).await["operation_id"]
        .as_str()
        .expect("the ref")
        .to_owned();

    let aborted = harness
        .allowed()
        .send(with_headers(
            "POST",
            &abort_path(&operation_id),
            None,
            &keyed("abort-1"),
        ))
        .await;

    assert_eq!(
        aborted.status(),
        StatusCode::BAD_REQUEST,
        "a completed run is not abortable, and the refusal is a lifecycle conflict rather \
         than a 500 saying the store is broken"
    );
    assert_eq!(problem_code(aborted).await, "LIFECYCLE_FORBIDDEN");
    let still = harness
        .allowed()
        .send(with_headers("GET", &import_path(&operation_id), None, &[]))
        .await;
    assert_eq!(
        body_json(still).await["state"],
        serde_json::json!("completed"),
        "and the refusal left it where it was"
    );
}

#[tokio::test]
async fn aborting_a_run_that_finished_with_conflicts_is_refused_too() {
    // **The hole the first abort case could not see** (D-294). It used a
    // `completed` run, where the state machine genuinely refuses. But a move to
    // the state a run is already IN returns early on both engines, so
    // `completed_with_conflicts` — the ordinary terminal state of any partially
    // conflicted import — would have been rewritten: a fresh `completed_at` and an
    // abort note stamped over a report where every row was attempted.
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;

    // A batch with one row that cannot commit: it asserts a version over a key
    // nothing holds, so Phase 2 conflicts it and the run ends with conflicts.
    let mut conflicting = row(plan, "eu", 1_500);
    conflicting["if_match"] = serde_json::json!(7);
    let submitted = harness
        .allowed()
        .send(with_headers(
            "POST",
            BULK_IMPORTS,
            Some(batch(&[conflicting])),
            &keyed("bulk-5"),
        ))
        .await;
    let body = body_json(submitted).await;
    assert_eq!(
        body["state"],
        serde_json::json!("completed_with_conflicts"),
        "the fixture has to reach that state or this case tests nothing: {body}"
    );
    let operation_id = body["operation_id"].as_str().expect("the ref").to_owned();

    let aborted = harness
        .allowed()
        .send(with_headers(
            "POST",
            &abort_path(&operation_id),
            None,
            &keyed("abort-2"),
        ))
        .await;
    assert_eq!(aborted.status(), StatusCode::BAD_REQUEST);
    // **Which** 400, matching the sibling abort case above: this route renders
    // several, and a bare status cannot say the state machine is what refused.
    assert_eq!(problem_code(aborted).await, "LIFECYCLE_FORBIDDEN");

    let still = body_json(
        harness
            .allowed()
            .send(with_headers("GET", &import_path(&operation_id), None, &[]))
            .await,
    )
    .await;
    assert!(
        still["report"].get("aborted").is_none(),
        "and no abort note was stamped over a report whose rows were all attempted: {still}"
    );
}

/// Hold an import run in `committing`, the one state the abort acts on.
///
/// The submit runs both phases synchronously, so no live batch can be caught
/// mid-flight; the repository is the only way to hold one there. Its repricing
/// sibling's `a_run_stalled_committing` says the same of its own plane.
async fn a_run_stalled_committing(harness: &Harness, client_key: &str) -> Uuid {
    let conn = harness.db.conn().expect("conn");
    let operation_id = Uuid::now_v7();
    bulk_repo::open(
        &conn,
        &harness.scope(),
        bss_pricing::infra::storage::repo::NewBulkOperation {
            operation_id,
            tenant_id: harness.tenant,
            kind: BulkKind::Import,
            client_key: client_key.to_owned(),
            request_hash: b"digest".to_vec(),
            report: serde_json::json!({ "rows": [] }),
            submitted_by: Uuid::from_u128(0xac_13),
            submitted_at: OffsetDateTime::now_utc(),
        },
    )
    .await
    .expect("open the run");
    bulk_repo::advance(
        &conn,
        &harness.scope(),
        harness.tenant,
        operation_id,
        BulkState::Validating,
        BulkState::Committing,
        serde_json::json!({ "rows": [] }),
        OffsetDateTime::now_utc(),
    )
    .await
    .expect("hold the run in committing");
    operation_id
}

/// **A caller of another tenant cannot abort this tenant's run**, and the refusal is
/// the one an unknown operation id gets.
///
/// The sibling read case's reason, on the verb that writes. `rest_authz`'s census
/// cannot reach this route — `drive` fills `{operationId}` with a fixed literal
/// `absent_ids` does not vary, so its `foreign` and `absent` requests are the same
/// bytes — and its by-id write loop is where a route that resolved its object before
/// narrowing on the tenant would otherwise be caught.
///
/// What the write costs is more than the read's: the sweep releases the run's row
/// locks and lands it terminal, so a foreign caller reaching it ends another
/// tenant's commit and unfreezes the rows that commit was holding.
#[tokio::test]
async fn a_foreign_tenants_run_cannot_be_aborted_and_the_owners_identical_call_can() {
    let harness = Harness::new().await;
    let operation_id = a_run_stalled_committing(&harness, "bulk-abort-cross-tenant").await;

    let foreign = harness
        .other_tenant()
        .send(with_headers(
            "POST",
            &abort_path(&operation_id.to_string()),
            None,
            &keyed("abort-foreign"),
        ))
        .await;
    let absent = harness
        .other_tenant()
        .send(with_headers(
            "POST",
            &abort_path(&Uuid::now_v7().to_string()),
            None,
            &keyed("abort-absent"),
        ))
        .await;

    let foreign_status = foreign.status();
    let absent_status = absent.status();
    assert!(
        !foreign_status.is_success(),
        "another tenant aborted this tenant's run"
    );
    assert_eq!(
        foreign_status, absent_status,
        "another tenant's run answers {foreign_status} and an unknown id {absent_status}; the \
         difference is a probe for which operation ids are real"
    );
    assert_eq!(
        body_json(foreign).await.get("type"),
        body_json(absent).await.get("type"),
        "and alike in the problem `type`, or the body distinguishes what the status does not"
    );

    // Read off the store and not off the response: a handler that swept and then
    // failed to render would answer an error over a run it had already ended.
    let stored = bulk_operation_row(&harness, operation_id).await;
    assert_eq!(
        stored.state,
        BulkState::Committing,
        "the refused abort left the run where it was: {stored:?}"
    );
    assert!(
        stored
            .report
            .get(bss_pricing::infra::bulk::ABORTED_MEMBER)
            .is_none(),
        "and stamped no abort note: {:?}",
        stored.report
    );

    // The positive control, and the whole route is under it: without a call that
    // succeeds, a route refusing every caller for an unrelated reason reads as
    // tenant isolation. `rest_support` grants `token_scopes(["*"])`, so the scopes
    // are not what separates these two callers — the tenant is.
    let owner = harness
        .allowed()
        .send(with_headers(
            "POST",
            &abort_path(&operation_id.to_string()),
            None,
            &keyed("abort-owner"),
        ))
        .await;
    assert_eq!(
        owner.status(),
        StatusCode::OK,
        "the run's own tenant aborts it, or the refusals above are about the request"
    );
    assert!(
        bulk_operation_row(&harness, operation_id)
            .await
            .report
            .get(bss_pricing::infra::bulk::ABORTED_MEMBER)
            .is_some(),
        "and the owner's abort is the one that swept"
    );
}

/// **An abort replayed under its own key is answered the run, not told it failed.**
///
/// The route reads a required `Idempotency-Key` for its refusal and binds nothing —
/// which leaves the key doing the opposite of its job. The first abort moves the run
/// out of `committing`; a client that timed out and retried sent the identical
/// request with the identical key and met the state guard, which answered **400
/// `LIFECYCLE_FORBIDDEN`** — a caller whose abort succeeded told it did not, the
/// precise conclusion the key exists to prevent.
///
/// The guard's own argument is kept whole rather than weakened, and the sibling
/// above is the control that proves it: a `completed_with_conflicts` run **that was
/// never aborted** is still refused, because a move to the state a run is already in
/// returns early and would stamp an abort note over a report where every row was
/// attempted. What distinguishes the two is not the state — an abort lands in that
/// same state — but the note the sweep writes, which is the only record of *this
/// operation having run*.
#[tokio::test]
async fn a_replayed_abort_is_answered_the_run_it_already_aborted() {
    let harness = Harness::new().await;

    let operation_id = a_run_stalled_committing(&harness, "bulk-abort-replay").await;

    let first = harness
        .allowed()
        .send(with_headers(
            "POST",
            &abort_path(&operation_id.to_string()),
            None,
            &keyed("abort-replay"),
        ))
        .await;
    assert_eq!(first.status(), StatusCode::OK, "the abort acts");
    let aborted = body_json(first).await;
    assert!(
        aborted["report"].get("aborted").is_some(),
        "and stamps the note that records it: {aborted}"
    );

    // The retry. The identical request, under the identical key.
    let replay = harness
        .allowed()
        .send(with_headers(
            "POST",
            &abort_path(&operation_id.to_string()),
            None,
            &keyed("abort-replay"),
        ))
        .await;
    assert_eq!(
        replay.status(),
        StatusCode::OK,
        "a caller whose abort succeeded is not told it failed"
    );
    let replayed = body_json(replay).await;
    assert_eq!(
        replayed["operation_id"],
        serde_json::json!(operation_id),
        "and is answered the run it aborted: {replayed}"
    );
    assert_eq!(
        replayed["completed_at"], aborted["completed_at"],
        "the second call is a read, not a second abort: nothing was re-stamped"
    );
}

#[tokio::test]
async fn the_refusal_names_the_run_and_the_run_serves_the_per_row_report() {
    // **The ref is a field, not a phrase** (D-295). Phase 1's entire value is the
    // per-row report, and the only thing pointing at it is the operation ref — so
    // a ref readable only by parsing an English sentence leaves that report
    // unreachable by every client that is not a person. The **shape** the GET
    // then serves is pinned here for the first time: `inst-bk-idem` makes the
    // stored report a wire contract, and until now no case named a field in it
    // beyond `committed`, so a rename would have been invisible.
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;

    let refused = harness
        .allowed()
        .send(with_headers(
            "POST",
            BULK_IMPORTS,
            Some(batch(&[row(plan, "eu", 1_500), row(plan, "eu", 2_500)])),
            &keyed("bulk-6"),
        ))
        .await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let problem = body_json(refused).await;
    assert_eq!(rest_support::code_in(&problem), "BULK_VALIDATION_FAILED");
    let operation_id = rest_support::violation_for(&problem, "operation_id")
        .unwrap_or_else(|| panic!("the refusal has to name the run it opened: {problem}"));

    let run = body_json(
        harness
            .allowed()
            .send(with_headers("GET", &import_path(&operation_id), None, &[]))
            .await,
    )
    .await;
    assert_eq!(
        run["state"],
        serde_json::json!("validation_failed"),
        "the ref the refusal handed back has to be the run that was refused: {run}"
    );

    let rows = run["report"]["rows"]
        .as_array()
        .unwrap_or_else(|| panic!("Phase 1's report is a list of rows: {run}"));
    assert!(!rows.is_empty(), "and it names the rows: {run}");
    for outcome in rows {
        assert!(
            outcome["row"].is_u64(),
            "each outcome carries its position: {outcome}"
        );
        let violations = outcome["violations"]
            .as_array()
            .unwrap_or_else(|| panic!("and every violation found against it: {outcome}"));
        assert!(!violations.is_empty(), "never an empty list: {outcome}");
        for violation in violations {
            assert!(violation["code"].is_string(), "{violation}");
            assert!(violation["detail"].is_string(), "{violation}");
        }
    }
    assert!(
        rows.iter().any(|outcome| {
            outcome["violations"].as_array().is_some_and(|found| {
                found
                    .iter()
                    .any(|v| v["code"] == serde_json::json!("DUPLICATE_SCOPE_KEY"))
            })
        }),
        "and the duplicate is what this batch was refused for: {run}"
    );
}

#[tokio::test]
async fn a_replay_of_a_refused_key_is_refused_again_and_imports_nothing() {
    // **A replay answers what the first call answered** (D-295). The refused
    // batch was a 400; a replay under the same key used to answer 202 with the
    // failed run, so a client that retried on a timeout — the exact client
    // idempotency is for — read the retry as having succeeded where the original
    // failed. The key is also **spent**, which the refusal now says: a corrected
    // batch under it imports nothing, and the third leg proves nothing landed.
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;

    let refused = harness
        .allowed()
        .send(with_headers(
            "POST",
            BULK_IMPORTS,
            Some(batch(&[row(plan, "eu", 1_500), row(plan, "eu", 2_500)])),
            &keyed("bulk-7"),
        ))
        .await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);

    // The same key, a **corrected** body — one row, no duplicate.
    let replayed = harness
        .allowed()
        .send(with_headers(
            "POST",
            BULK_IMPORTS,
            Some(batch(&[row(plan, "eu", 1_500)])),
            &keyed("bulk-7"),
        ))
        .await;
    assert_eq!(
        replayed.status(),
        StatusCode::BAD_REQUEST,
        "a replay may not answer 202 where the first call answered 400"
    );
    assert_eq!(problem_code(replayed).await, "BULK_VALIDATION_FAILED");

    // And it imported nothing: the same row under a *fresh* key claims the key
    // outright, which it could not do had the replay written it.
    let fresh = harness
        .allowed()
        .send(with_headers(
            "POST",
            BULK_IMPORTS,
            Some(batch(&[row(plan, "eu", 1_500)])),
            &keyed("bulk-7-corrected"),
        ))
        .await;
    assert_eq!(fresh.status(), StatusCode::ACCEPTED);
    let landed = body_json(fresh).await;
    assert_eq!(
        landed["report"]["committed"]
            .as_array()
            .expect("an array")
            .len(),
        1,
        "the corrected batch lands under its own key, so the replay wrote nothing: {landed}"
    );
}

#[tokio::test]
async fn the_commit_report_names_its_rows_by_field() {
    // The other half of the wire contract `inst-bk-idem` pins (D-295): a run that
    // committed one row and conflicted another. Both arms are asserted by field
    // name, because a report whose readers are all inside this crate today
    // becomes the operator's only record of what an import did.
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;

    let mut conflicting = row(plan, "us", 2_500);
    conflicting["if_match"] = serde_json::json!(7);
    let submitted = harness
        .allowed()
        .send(with_headers(
            "POST",
            BULK_IMPORTS,
            Some(batch(&[row(plan, "eu", 1_500), conflicting])),
            &keyed("bulk-8"),
        ))
        .await;
    assert_eq!(submitted.status(), StatusCode::ACCEPTED);
    let run = body_json(submitted).await;
    assert_eq!(
        run["state"],
        serde_json::json!("completed_with_conflicts"),
        "the fixture has to reach both arms or this case pins one: {run}"
    );

    let committed = run["report"]["committed"]
        .as_array()
        .unwrap_or_else(|| panic!("the committed arm: {run}"));
    assert_eq!(committed.len(), 1, "{run}");
    assert_eq!(committed[0]["row"], serde_json::json!(0));
    assert!(
        committed[0]["price_id"].is_string(),
        "a committed row names the draft it wrote: {run}"
    );

    let conflicted = run["report"]["conflicted"]
        .as_array()
        .unwrap_or_else(|| panic!("the conflicted arm: {run}"));
    assert_eq!(conflicted.len(), 1, "{run}");
    assert_eq!(
        conflicted[0]["row"],
        serde_json::json!(1),
        "and it is the row that conflicted, not the one that landed: {run}"
    );
    let violations = conflicted[0]["violations"]
        .as_array()
        .unwrap_or_else(|| panic!("carrying its violation: {run}"));
    assert_eq!(violations.len(), 1, "{run}");
    assert!(violations[0]["code"].is_string(), "{run}");
    assert!(violations[0]["detail"].is_string(), "{run}");
}

#[tokio::test]
async fn an_abort_without_an_idempotency_key_is_refused() {
    let harness = Harness::new().await;
    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            &abort_path(&Uuid::now_v7().to_string()),
            None,
            &[],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

/// Two usage rows differing **only** in their meter are two keys, and a batch
/// carrying both must not be refused as a duplicate (D-306).
///
/// `ScopeKeyRequest` carries six axes and the `(meter, dimensionKey)` pair is
/// authored on the *content* (D-196 clause (3)), so `rows_of` built a **line-less**
/// key until this case existed — and `duplicate_scope_keys` then grouped these two
/// rows as one key and refused the batch, which the very sentence in its own doc
/// says is wrong: *"Two rows differing in their meter or dimension key are **not**
/// this case: those are two keys and both author."*
///
/// D-103's confirmed shape is the fixture: a plan pricing several meters is one
/// plan, not three.
///
/// The kind is `per_unit` — the plain untiered metered rate, unit price times
/// metered `Q`. It was `flat` until D-312's bulk arm was built, and `flat` is in no
/// part of the usage set: the fixture had been carrying a key contradiction, which
/// the import now refuses per-row in Phase 1 rather than at publish. That is the
/// same latent defect the arm exists to catch, found in the fixture of the case that
/// proves two meters are two keys.
fn usage_row(plan_id: Uuid, meter: &str, amount: i64) -> serde_json::Value {
    serde_json::json!({
        "plan_id": plan_id,
        "scope_key": {
            "currency": "USD",
            "region": "eu",
            "phase": rest_support::seeded_phase().get().to_string(),
            "price_eligibility": "all_subscriptions",
            "charge_kind": "usage",
            "cohort": serde_json::Value::Null
        },
        "content": {
            "model_kind": "per_unit",
            "amount_minor": amount,
            "tax_inclusive": false,
            "meter": meter,
            "billing_timing": "arrears",
            "rounding_policy_ref": "half_up"
        }
    })
}

#[tokio::test]
async fn two_usage_rows_on_different_meters_are_two_keys_and_both_author() {
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            BULK_IMPORTS,
            Some(batch(&[
                usage_row(plan, "cloudlets", 1_500),
                usage_row(plan, "egress-gb", 2_500),
            ])),
            &keyed("bulk-meters"),
        ))
        .await;

    assert_eq!(
        response.status(),
        StatusCode::ACCEPTED,
        "a batch of two meters is not a duplicate batch"
    );
    let run = body_json(response).await;
    assert_eq!(
        run["report"]["committed"]
            .as_array()
            .expect("an array")
            .len(),
        2,
        "and both rows author, on keys the store files them under separately: {run}"
    );
}

/// A usage row whose kind is `flat`, which is in no part of the usage set.
///
/// The shape the fixture above carried until D-312's bulk arm was built.
fn contradicting_usage_row(plan_id: Uuid, meter: &str, amount: i64) -> serde_json::Value {
    let mut built = usage_row(plan_id, meter, amount);
    built["content"]["model_kind"] = serde_json::json!("flat");
    built
}

#[tokio::test]
async fn a_batch_whose_row_contradicts_its_own_key_is_refused_and_commits_nothing() {
    // **D-312's third door, over HTTP.** `POST` and `PATCH` refuse a key
    // contradiction on arrival; `bulk_imports` is the third door a price row enters
    // through and it did not, so a batch carrying `flat` on a `usage` key committed
    // and was refused only at publish — after the rows were written, through the one
    // door of the three that writes them in bulk.
    //
    // The refusal is Phase 1's, so it is the batch-level 400 with the per-row report
    // at the GET, exactly as the in-batch duplicate is. Nothing about the code is
    // minted here: the violation carries the rule's own
    // `MODEL_KIND_CHARGEKIND_MISMATCH`.
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;

    let refused = harness
        .allowed()
        .send(with_headers(
            "POST",
            BULK_IMPORTS,
            Some(batch(&[
                contradicting_usage_row(plan, "cloudlets", 1_500),
                usage_row(plan, "egress-gb", 2_500),
            ])),
            &keyed("bulk-key-contradiction"),
        ))
        .await;

    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let problem = body_json(refused).await;
    assert_eq!(rest_support::code_in(&problem), "BULK_VALIDATION_FAILED");
    let operation_id = rest_support::violation_for(&problem, "operation_id")
        .unwrap_or_else(|| panic!("the refusal has to name the run it opened: {problem}"));

    let run = body_json(
        harness
            .allowed()
            .send(with_headers("GET", &import_path(&operation_id), None, &[]))
            .await,
    )
    .await;
    assert_eq!(run["state"], serde_json::json!("validation_failed"));
    assert!(
        run["report"]["committed"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "all-or-nothing: a blocked batch commits no row at all: {run}"
    );

    let rows = run["report"]["rows"]
        .as_array()
        .unwrap_or_else(|| panic!("Phase 1's report is a list of rows: {run}"));
    assert_eq!(
        rows.iter()
            .map(|outcome| &outcome["row"])
            .collect::<Vec<_>>(),
        vec![&serde_json::json!(0)],
        "row 0 is the contradiction and row 1 is a legal usage row — a report that \
         refused the batch entire would pass a weaker assertion than this: {run}"
    );
    assert!(
        rows[0]["violations"].as_array().is_some_and(|found| found
            .iter()
            .any(|v| v["code"] == serde_json::json!("MODEL_KIND_CHARGEKIND_MISMATCH"))),
        "and the code is the rule's own, inherited rather than minted for the import: {run}"
    );
}

#[tokio::test]
async fn a_metered_draft_is_found_by_the_batch_that_names_its_key() {
    // The other half of D-306, and the two are told apart by **which** conflict
    // answers. With a line-less key `draft_rows`' lookup could never match a
    // metered draft, so the row took the *create* path, the store derived the line
    // for itself and the collision came back `DUPLICATE_SCOPE_KEY` — a bulk import
    // could not edit a metered row at all. With the line on the key the lookup
    // finds it, and what answers is the per-row conflict about a **missing
    // version**, which is a different sentence and a different remedy.
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;

    let first = harness
        .allowed()
        .send(with_headers(
            "POST",
            BULK_IMPORTS,
            Some(batch(&[usage_row(plan, "cloudlets", 1_500)])),
            &keyed("meter-create"),
        ))
        .await;
    assert_eq!(first.status(), StatusCode::ACCEPTED);

    // The same key again, with no `if_match`: the draft is there, so the answer is
    // "re-read it and resubmit with its ETag", not the store's duplicate-key
    // refusal.
    let second = harness
        .allowed()
        .send(with_headers(
            "POST",
            BULK_IMPORTS,
            Some(batch(&[usage_row(plan, "cloudlets", 9_900)])),
            &keyed("meter-edit"),
        ))
        .await;
    assert_eq!(second.status(), StatusCode::ACCEPTED);
    let run = body_json(second).await;

    let violation = &run["report"]["conflicted"][0]["violations"][0];
    let detail = violation["detail"]
        .as_str()
        .unwrap_or_else(|| panic!("the row conflicted and says why: {run}"));
    assert!(
        detail.contains("already holds this key"),
        "the lookup found the metered draft: {detail}"
    );
    // **The create path is the other outcome, so it is what the claim is about.**
    // Reading `DUPLICATE_SCOPE_KEY` out of `detail` said nothing - no code is ever
    // written there - and asserting `code == BULK_ROW_CONFLICT` says no more:
    // `infra::bulk::conflicted` is the sole constructor of anything in
    // `report.conflicted` and hard-codes that code, so the equality holds for every
    // row that reached this array at all. A row that fell through to the create
    // path would be in `committed` instead, and that is the observable.
    assert_eq!(
        run["report"]["committed"].as_array().map(Vec::len),
        Some(0),
        "the row conflicted on the lookup and did not fall through to the create path: {run}"
    );
    assert_eq!(
        run["report"]["conflicted"].as_array().map(Vec::len),
        Some(1),
        "exactly the submitted row conflicted: {run}"
    );

    // And nothing was written: the second submission left the first draft's amount
    // standing, so the conflict is a refusal rather than an edit that also reported
    // one.
    let rows = rest_support::price_rows(&harness, plan).await;
    assert_eq!(
        rows.len(),
        1,
        "the conflicted run created no second row: {rows:?}"
    );
}

// ---------------------------------------------------------------------------
// Z11-4 — a Phase-1 store failure, and the key it spends.
// ---------------------------------------------------------------------------

/// Make the store-dependent half of Phase 1 fail, and nothing else.
///
/// `classify_against_store` reads the plan's **published** rows and rehydrates
/// each one's canonical key; `to_scope_key` refuses a `currency` the domain cannot
/// parse with `CorruptRow`. `pricing_price.currency` is a bare `varchar(3)` with
/// no CHECK on its shape, so a two-letter code is a value the store admits and the
/// crate refuses — a row a restore, a data fix or another shape's migration could
/// leave behind.
///
/// It is injected on a row of its **own** key rather than the batch's, so nothing
/// here depends on the batch colliding with it: what fails is the plan-wide read,
/// which is the whole of Phase 1's store half.
async fn poison_the_published_read(harness: &Harness, plan_id: Uuid) {
    let conn = harness.db.conn().expect("conn");
    let scope = harness.scope();
    let row = price::ActiveModel {
        price_id: Set(Uuid::now_v7()),
        tenant_id: Set(harness.tenant),
        plan_id: Set(plan_id),
        currency: Set("US".to_owned()),
        region: Set("EU".to_owned()),
        phase: Set(rest_support::seeded_phase().get()),
        charge_kind: Set("recurring".to_owned()),
        amount_minor: Set(Some(1_000)),
        model_kind: Set(Some("flat".to_owned())),
        lifecycle_state: Set("published".to_owned()),
        created_by: Set(Uuid::from_u128(0xac_11)),
        created_at_utc: Set(OffsetDateTime::now_utc()),
        ..price::ActiveModel::default()
    };
    price::Entity::insert(row.clone())
        .secure()
        .scope_with_model(&scope, &row)
        .expect("scope the seed")
        .exec(&conn)
        .await
        .expect("the currency column admits a code the domain does not");
}

/// **The RED this case is about.** The run is born `validating`
/// (`bulk_repo::open`), and `classify_against_store` carried a bare `?`. A
/// transient repo error there left the run in `validating` **forever**: `abort`
/// refuses anything that is not `committing`, no sweeper exists, and the only two
/// other writers of a run's state are the rule path's `ValidationFailed` and Phase
/// 2's `Committing`, neither of which is reached.
///
/// The client key is spent by then, so the replay is worse than the failure: the
/// replay arm answers `202` with the run's placeholder report — `state:
/// "validating"`, `report: {"rows": []}` — telling a client that resubmitted on a
/// timeout that its import succeeded, for a run that imported nothing. That is the
/// one conclusion idempotency exists to prevent, and the same handler argues it at
/// length twenty lines up, where it installs a refusal for the `ValidationFailed`
/// case.
///
/// Phase 2 was hardened three times for this hazard (D-294, D-297, D-300) and
/// Phase 1 not once; `domain::bulk`'s own `Rejected` exists because D-267 found a
/// run "stranded there with no exit".
///
/// Armed at both halves: the run must be left terminal, and the replay must be a
/// refusal. A case that only asserted the first `POST` failed would pass under the
/// bare `?`.
#[tokio::test]
async fn a_phase_one_store_failure_lands_the_run_terminal_and_the_replay_is_refused() {
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;
    poison_the_published_read(&harness, plan).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            BULK_IMPORTS,
            Some(batch(&[row(plan, "eu", 1_500)])),
            &keyed("bulk-phase1-fault"),
        ))
        .await;
    assert!(
        response.status().is_server_error(),
        "a store fault in Phase 1 is the run's failure and reaches the caller as one, not as \
         a validation refusal: {}",
        response.status()
    );

    // The run itself, read past the API because the failing response carries no
    // operation ref to `GET`.
    let conn = harness.db.conn().expect("conn");
    let run = bulk_repo::find_by_client_key(
        &conn,
        &harness.scope(),
        harness.tenant,
        BulkKind::Import,
        "bulk-phase1-fault",
    )
    .await
    .expect("read the run by its key")
    .expect("the key opened a run, which is exactly why it must not be left mid-flight");
    assert_eq!(
        run.state,
        BulkState::ValidationFailed,
        "the run this key spent has to reach a state something can act on; `validating` has \
         no exit at all — abort refuses it and no sweeper exists: {run:?}"
    );
    assert!(
        run.completed_at.is_some(),
        "and a terminal run is stamped, which the CHECK pairs with the state: {run:?}"
    );
    assert!(
        run.report.get("failure").is_some(),
        "the report says why, because the GET is where a batch's answer lives: {}",
        run.report
    );

    // The replay. Under the bare `?` this answered 202 with `{"rows": []}` — a
    // resubmit told it succeeded.
    let replay = harness
        .allowed()
        .send(with_headers(
            "POST",
            BULK_IMPORTS,
            Some(batch(&[row(plan, "eu", 1_500)])),
            &keyed("bulk-phase1-fault"),
        ))
        .await;
    assert_eq!(
        replay.status(),
        StatusCode::BAD_REQUEST,
        "a replay must not tell a client a resubmit succeeded where the original failed"
    );
    assert_eq!(
        problem_code(replay).await,
        "BULK_VALIDATION_FAILED",
        "and it is refused under the code the run's own state names"
    );
}

// ---------------------------------------------------------------------------
// Z11-6 — a row's content faults are the run's per-row report, not a bare 400.
// ---------------------------------------------------------------------------

/// One row whose **key** and whose **content** are both wrong.
///
/// Two faults from two different derivations, which is the arrangement the finding
/// names: `CurrencyCode::new` is `scope_key_of`'s and `MinorAmount::new` is
/// `content_of`'s, so a handler that chained them through `?` could only ever
/// report one of them however it ordered the two.
fn doubly_bad_row(plan_id: Uuid) -> serde_json::Value {
    serde_json::json!({
        "plan_id": plan_id,
        "scope_key": {
            "currency": "US",
            "region": "eu",
            "phase": rest_support::seeded_phase().get().to_string(),
            "price_eligibility": "all_subscriptions",
            "charge_kind": "recurring",
            "cohort": serde_json::Value::Null
        },
        "content": {
            "model_kind": "flat",
            "amount_minor": -5,
            "tax_inclusive": false
        }
    })
}

/// **The RED this case is about.** `rows_of` collected through
/// `Result<Vec<_>, DomainError>`, so the first bad row aborted the whole body with
/// one refusal carrying no index — and it ran *before* `bulk_repo::open`, so no run
/// held a report for it and the `GET` had nothing to serve. A thousand-row batch
/// with a typo'd currency in row 700 was answered "currency invalid: US".
///
/// Against the contract twice over: this route promises "Phase 1 validates the
/// whole batch and refuses it if any row is invalid … the per-row report is on the
/// run", and `domain::import` states the posture — "a rule that can answer for a
/// row **must** answer for every row rather than stopping at the first".
///
/// Armed at all three halves, because each can pass while the others fail: the
/// refusal is the Phase-1 one and names its run, the run holds the row's index, and
/// **both** of the row's faults are against it.
#[tokio::test]
async fn a_row_that_is_not_a_readable_price_row_is_reported_against_its_index() {
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;

    let refused = harness
        .allowed()
        .send(with_headers(
            "POST",
            BULK_IMPORTS,
            Some(batch(&[row(plan, "eu", 1_500), doubly_bad_row(plan)])),
            &keyed("bulk-unreadable-row"),
        ))
        .await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let problem = body_json(refused).await;
    assert_eq!(
        rest_support::code_in(&problem),
        "BULK_VALIDATION_FAILED",
        "section 5 makes any Phase-1 failure this code, and a content fault is one: {problem}"
    );
    let operation_id = rest_support::violation_for(&problem, "operation_id")
        .unwrap_or_else(|| panic!("the refusal has to name the run it opened: {problem}"));

    let run = body_json(
        harness
            .allowed()
            .send(with_headers("GET", &import_path(&operation_id), None, &[]))
            .await,
    )
    .await;
    assert_eq!(
        run["state"],
        serde_json::json!("validation_failed"),
        "the run this key spent has to reach a state something can act on: {run}"
    );

    let unreadable = run["report"]["unreadable"]
        .as_array()
        .unwrap_or_else(|| panic!("the run holds which rows could not be read: {run}"));
    assert_eq!(
        unreadable.len(),
        1,
        "one entry, for the one row at fault - row 0 is a perfectly good row and naming it \
         would be the whole-batch refusal this replaces: {run}"
    );
    assert_eq!(
        unreadable[0]["row"],
        serde_json::json!(1),
        "and it is named by its position in the submitted batch: {run}"
    );

    let faults = unreadable[0]["faults"]
        .as_array()
        .unwrap_or_else(|| panic!("with every fault found against it: {run}"))
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(
        faults.contains("currency"),
        "the key's fault has to be in the report: {faults}"
    );
    assert!(
        faults.contains("amount"),
        "and so does the content's - reporting one of the two is what stopping at the first \
         fault looked like: {faults}"
    );

    // Nothing was imported, which is what "refuses it if any row is invalid" means.
    let conn = harness.db.conn().expect("conn");
    let stored = price::Entity::find()
        .secure()
        .scope_with(&harness.scope())
        .all(&conn)
        .await
        .expect("read pricing_price");
    assert!(
        stored.is_empty(),
        "an all-or-nothing Phase 1 commits nothing: {stored:?}"
    );
}

/// The replay of a key spent on an unreadable batch is refused, not answered `202`.
///
/// The positive control for landing the run terminal at all. The run is opened
/// before the rows are read now, so the key **is** spent — and a run left
/// `validating` would be answered `202` with `{"rows": []}` on the retry, telling a
/// client that resubmitted on a timeout that its import succeeded. That is the
/// conclusion D-295's refusal exists to prevent, and Z11-4 found this exact shape
/// on the store-fault path.
#[tokio::test]
async fn a_replay_of_a_key_spent_on_an_unreadable_batch_is_refused() {
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;

    for _ in 0..2 {
        let response = harness
            .allowed()
            .send(with_headers(
                "POST",
                BULK_IMPORTS,
                Some(batch(&[doubly_bad_row(plan)])),
                &keyed("bulk-unreadable-replay"),
            ))
            .await;
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "neither the first call nor its replay may read as a success"
        );
        assert_eq!(
            problem_code(response).await,
            "BULK_VALIDATION_FAILED",
            "and the replay is refused under the code the run's own state names"
        );
    }
}

// ---------------------------------------------------------------------------
// Z11-7 — what actually bounds a submitted batch.
// ---------------------------------------------------------------------------

/// **The bound that exists, measured rather than asserted in prose.**
///
/// The finding says the batch is unbounded, and no *row* cap exists — that absence
/// is recorded as deliberate in the module doc, because the nearest stated number
/// (§1.2's 500 rows/plan) is a **soft, advisory** publish-time cap by D-160 and a
/// batch spans plans, so adopting it as a hard request refusal would refuse batches
/// the design set admits and contradict the decision that made it advisory.
///
/// What is not absent is a bound in **bytes**: the extractor this route reads its
/// body through carries the platform default, so a body past it is refused before
/// any handler code runs — no run opened, no key spent, no lock taken. This case
/// pins that, because the module doc now claims it, and a claim about a limit
/// nobody measured is how "unbounded" got written in the first place.
///
/// Armed against the claim and not around it: the body is built one row over the
/// limit and the batch is otherwise **valid** - a distinct region per row, and a
/// three-row control of the same shape is submitted under the limit and asserted to
/// commit clean - so a `413` cannot be a validation refusal wearing another status.
#[tokio::test]
async fn a_body_past_the_platform_limit_is_refused_before_the_handler_runs() {
    /// The platform default this route inherits.
    const LIMIT: usize = 2 * 1024 * 1024;

    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;

    // Rows are appended until the serialized body is over the limit, and the
    // premise is then asserted. Measured rather than computed from an assumed row
    // size, so a change to `BulkImportRowRequest`'s shape cannot leave this case
    // building a body that is under the limit and asserting it is over.
    // **A distinct region per row**, because the claim above is that the batch is
    // otherwise valid. One region repeated gives every row the identical canonical
    // scope key, which is the in-batch-duplicate batch
    // `a_duplicate_inside_the_batch_blocks_it_and_commits_nothing` proves is refused
    // 400 `BULK_VALIDATION_FAILED` - and a batch refused for that reason leaves the
    // 413 indistinguishable from a validation refusal, with only the
    // run-not-opened half separating them.
    let mut rows: Vec<serde_json::Value> = Vec::new();
    let mut size = 0;
    while size <= LIMIT {
        let one = row(plan, &format!("eu{}", rows.len()), 1_500);
        size += one.to_string().len();
        rows.push(one);
    }
    let body = batch(&rows);
    assert!(
        body.to_string().len() > LIMIT,
        "the premise of this case: the body has to be over the limit it is testing"
    );

    // The other half of the premise, **measured**: the same row shape, under the
    // limit and on a plan of its own so it collides with nothing, is accepted and
    // commits every row. Without this the "otherwise valid" claim would be prose
    // again, and prose is what put the false one here.
    let control_plan = Uuid::now_v7();
    seed_current_plan(&harness, control_plan).await;
    let control_rows: Vec<serde_json::Value> = (0..3)
        .map(|i| row(control_plan, &format!("eu{i}"), 1_500))
        .collect();
    let control = harness
        .allowed()
        .send(with_headers(
            "POST",
            BULK_IMPORTS,
            Some(batch(&control_rows)),
            &keyed("bulk-under-the-body-limit"),
        ))
        .await;
    assert_eq!(control.status(), StatusCode::ACCEPTED);
    let control_report = body_json(control).await;
    assert_eq!(
        control_report["report"]["committed"]
            .as_array()
            .map(Vec::len),
        Some(3),
        "the rows this case sends are valid rows: {control_report}"
    );
    assert_eq!(
        control_report["report"]["conflicted"]
            .as_array()
            .map(Vec::len),
        Some(0),
        "and they collide with nothing: {control_report}"
    );

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            BULK_IMPORTS,
            Some(body),
            &keyed("bulk-over-the-body-limit"),
        ))
        .await;
    assert_eq!(
        response.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "the platform body limit is what bounds this route's batch today"
    );

    // And it is refused *before* the handler: no run was opened, so the key is not
    // spent. This is the half that makes the bound harmless rather than a trap.
    let conn = harness.db.conn().expect("conn");
    let opened = bulk_repo::find_by_client_key(
        &conn,
        &harness.scope(),
        harness.tenant,
        BulkKind::Import,
        "bulk-over-the-body-limit",
    )
    .await
    .expect("read the run by its key");
    assert!(
        opened.is_none(),
        "a body refused by the extractor never reaches the handler that opens a run: {opened:?}"
    );
}

// ---------------------------------------------------------------------------
// The gate, per pair (Task A).
//
// **All three surfaces asked for `historical_import` until 2026-08-17, and three
// independent accounts said `plan`**: S5 §3's read row names
// `GET /bss-pricing/v1/bulk-imports/{id}` under `plan × read`; its bulk row files
// the mutating pair under *"the **same** `plan × write` / `publish` — bulk is
// authoring at scale (and abort is un-authoring at scale), no new authority"*; and
// `write_scope`'s own doc comment, four lines above the body that contradicted it,
// said "the `plan x write` gate both mutating surfaces take".
//
// The consequence was not cosmetic. `historical_import` was the **restricted**
// backdating grant — S5 §3 step 5, "never included in a default role" — so the
// three roles S5's matrix gives `plan × write` (ProductManager, FinanceManager,
// CatalogAdmin) were answered **403** on a plane the same table hands them, and
// the label is struck outright now (D-330).
//
// Why these cases and not the census: `rest_authz.rs` pins the pair each route
// asks the PDP, which is the exact instrument, but both sides of it are
// descriptions of one implementation and it was green through the whole defect.
// What no fixture above `SelectiveResolver` can see is the operator-visible fact —
// a caller holding **only** the catalogued pair, and nothing else, gets served.
// Each refusal below is paired with that positive control, because a refusal alone
// passes just as well when the deny is coming from somewhere other than the gate
// under test.
// ---------------------------------------------------------------------------

/// The operator every case below acts as.
const BULK_OPERATOR: Uuid = Uuid::from_u128(0xb0_1c);

/// `plan × write` **alone** submits a batch — the entrance S5 §3's bulk row
/// promises, and the one that answered 403.
///
/// Not merely "not forbidden": the run is driven to `202` and the rows are read
/// back off the `GET`, so a gate that admitted the caller and then failed to reach
/// the store could not pass this.
#[tokio::test]
async fn a_caller_holding_only_plan_write_submits_a_bulk_import() {
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;

    let response = harness
        .selectively_allowed_as(BULK_OPERATOR, &[(labels::PLAN, actions::WRITE)])
        .send(with_headers(
            "POST",
            BULK_IMPORTS,
            Some(batch(&[row(plan, "eu", 1_500)])),
            &keyed("bulk-authz-plan-write"),
        ))
        .await;

    assert_eq!(
        response.status(),
        StatusCode::ACCEPTED,
        "plan x write alone, with no historical_import grant at all, authorizes the submit"
    );
    let body = body_json(response).await;
    assert_eq!(body["state"], serde_json::json!("completed"));
    assert_eq!(
        body["report"]["committed"]
            .as_array()
            .expect("an array")
            .len(),
        1,
        "and the row landed rather than the gate merely letting the request past: {body}"
    );
}

/// `plan × read` **alone** reads a run.
///
/// The batch is submitted by the ordinary admin client: what this case is about is
/// the `GET`'s gate, not the `POST`'s.
#[tokio::test]
async fn a_caller_holding_only_plan_read_reads_a_bulk_import() {
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;
    let submitted = harness
        .allowed()
        .send(with_headers(
            "POST",
            BULK_IMPORTS,
            Some(batch(&[row(plan, "eu", 1_500)])),
            &keyed("bulk-authz-plan-read"),
        ))
        .await;
    let operation_id = body_json(submitted).await["operation_id"]
        .as_str()
        .expect("the ref")
        .to_owned();

    let response = harness
        .selectively_allowed_as(BULK_OPERATOR, &[(labels::PLAN, actions::READ)])
        .send(with_headers("GET", &import_path(&operation_id), None, &[]))
        .await;

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "plan x read alone authorizes the run's journal - a run's report is price data"
    );
    let run = body_json(response).await;
    assert_eq!(
        run["operation_id"],
        serde_json::json!(operation_id),
        "and it is the run that was asked for: {run}"
    );
}

/// `plan × write` **alone** reaches the abort, which is `un-authoring at scale`
/// and carries no authority of its own.
///
/// The run is absent on purpose, so what this case reads is the **404 the store
/// answers past the gate** rather than a 403 in front of it. Driving a real
/// `committing` run through HTTP is not reachable from a route suite — Phase 2
/// completes inside the submit — and `aborting_a_finished_run_...` above already
/// owns the lifecycle refusal.
#[tokio::test]
async fn a_caller_holding_only_plan_write_reaches_the_abort() {
    let harness = Harness::new().await;

    let response = harness
        .selectively_allowed_as(BULK_OPERATOR, &[(labels::PLAN, actions::WRITE)])
        .send(with_headers(
            "POST",
            &abort_path(&Uuid::now_v7().to_string()),
            None,
            &keyed("bulk-authz-abort"),
        ))
        .await;

    assert_ne!(
        response.status(),
        StatusCode::FORBIDDEN,
        "plan x write is the abort's whole gate: S5 section 3 gives it no authority of its own"
    );
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "and it got past the gate to the store, which has no such run"
    );
}

/// The read grant does **not** carry the submit. The mutating gate is
/// `plan × write` specifically, not "some pair on `plan`".
#[tokio::test]
async fn plan_read_alone_does_not_authorize_the_submit() {
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;

    let response = harness
        .selectively_allowed_as(BULK_OPERATOR, &[(labels::PLAN, actions::READ)])
        .send(with_headers(
            "POST",
            BULK_IMPORTS,
            Some(batch(&[row(plan, "eu", 1_500)])),
            &keyed("bulk-authz-read-only"),
        ))
        .await;

    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "plan x read must not author at scale"
    );
    let conn = harness.db.conn().expect("conn");
    let opened = bulk_repo::find_by_client_key(
        &conn,
        &harness.scope(),
        harness.tenant,
        BulkKind::Import,
        "bulk-authz-read-only",
    )
    .await
    .expect("read the run by its key");
    assert!(
        opened.is_none(),
        "and the refusal opened no run and spent no key: {opened:?}"
    );
}

/// The write grant does **not** carry the read. `plan × read` is the pair S5 §3's
/// read row names for this path, and the `GET` asks for it rather than for
/// whatever the caller happens to hold on `plan`.
#[tokio::test]
async fn plan_write_alone_does_not_authorize_the_read() {
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;
    let submitted = harness
        .allowed()
        .send(with_headers(
            "POST",
            BULK_IMPORTS,
            Some(batch(&[row(plan, "eu", 1_500)])),
            &keyed("bulk-authz-write-only"),
        ))
        .await;
    let operation_id = body_json(submitted).await["operation_id"]
        .as_str()
        .expect("the ref")
        .to_owned();

    let response = harness
        .selectively_allowed_as(BULK_OPERATOR, &[(labels::PLAN, actions::WRITE)])
        .send(with_headers("GET", &import_path(&operation_id), None, &[]))
        .await;

    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "the read asks for plan x read, not for any pair on plan"
    );
}

/// A **neighbouring** authoring grant confers nothing here. `bundle × write`
/// authors a composition (S8) and `config × write` declares the taxonomies
/// (D-120); neither is the plan plane, and bulk is authoring on the plan plane.
///
/// This is the case that would still pass if the bulk gate were left on any label
/// at all, which is why it stands beside the three positives rather than instead
/// of them.
#[tokio::test]
async fn a_neighbouring_authoring_grant_does_not_authorize_the_bulk_plane() {
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;

    let response = harness
        .selectively_allowed_as(
            BULK_OPERATOR,
            &[
                (labels::BUNDLE, actions::WRITE),
                (labels::CONFIG, actions::WRITE),
            ],
        )
        .send(with_headers(
            "POST",
            BULK_IMPORTS,
            Some(batch(&[row(plan, "eu", 1_500)])),
            &keyed("bulk-authz-neighbour"),
        ))
        .await;

    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "bundle x write and config x write are not the plan plane"
    );
}

/// **A caller of another tenant cannot read this tenant's run**, and the refusal is
/// the one an unknown operation id gets.
///
/// The only shape that exercises the SQL tenant predicate on this route.
/// `rest_authz`'s census cannot: `drive` fills `{operationId}` with a fixed literal
/// `absent_ids` does not vary, so its `foreign` and `absent` requests are the same
/// bytes and the route is listed in its `BY_ID_READS_THIS_FIXTURE_CANNOT_STAGE`.
///
/// What the body carries is why it matters: the report names every committed key —
/// plan id, currency, region, phase — so a read that resolved the run before
/// narrowing hands another tenant's price book over rather than merely confirming a
/// run exists.
#[tokio::test]
async fn a_foreign_tenants_run_reads_like_an_unknown_one() {
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;

    let opened = harness
        .allowed()
        .send(with_headers(
            "POST",
            BULK_IMPORTS,
            Some(batch(&[row(plan, "eu", 1_500)])),
            &keyed("cross-tenant-1"),
        ))
        .await;
    assert_eq!(
        opened.status(),
        StatusCode::ACCEPTED,
        "the fixture needs a run"
    );
    let operation_id = body_json(opened).await["operation_id"]
        .as_str()
        .expect("the ref")
        .to_owned();

    let foreign = harness
        .other_tenant()
        .send(with_headers("GET", &import_path(&operation_id), None, &[]))
        .await;
    let absent = harness
        .other_tenant()
        .send(with_headers(
            "GET",
            &import_path(&Uuid::now_v7().to_string()),
            None,
            &[],
        ))
        .await;

    let foreign_status = foreign.status();
    let absent_status = absent.status();
    assert!(
        !foreign_status.is_success(),
        "another tenant read this tenant's run and the keys it committed"
    );
    assert_eq!(
        foreign_status, absent_status,
        "another tenant's run answers {foreign_status} and an unknown id {absent_status}; the \
         difference is a probe for which operation ids are real"
    );
    assert_eq!(
        body_json(foreign).await.get("type"),
        body_json(absent).await.get("type"),
        "and alike in the problem `type`, or the body distinguishes what the status does not"
    );

    // The control. Without it a route that refused every reader would satisfy the
    // two assertions above.
    let owner = harness
        .allowed()
        .send(with_headers("GET", &import_path(&operation_id), None, &[]))
        .await;
    assert_eq!(
        owner.status(),
        StatusCode::OK,
        "the run's own tenant reads it, or the refusals above are about the request"
    );
    assert_eq!(
        body_json(owner).await["operation_id"],
        serde_json::json!(operation_id)
    );
}
