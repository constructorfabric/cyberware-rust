//! Resolution budgets at the worker boundary, including rollback after writes.

use sea_orm::EntityTrait;
use toolkit_db::secure::SecureEntityExt;

use super::*;
use types_registry::config::ByteSize;
use types_registry::domain::admission::AdmissionFailureReason;
use types_registry::domain::admission::fingerprint::canonical_text;
use types_registry::infra::storage::entity::type_schema_revision;

fn refused(outcome: &OperationOutcome, reason: &AdmissionFailureReason) {
    let item = &outcome.items[0];
    assert_eq!(item.status, OperationItemStatus::Failed, "{item:?}");
    assert_eq!(&item.failure.as_ref().expect("reason").reason, reason);
    assert!(item.resource_version.is_none());
    assert!(item.revision_no.is_none());
}

async fn no_entity(db: &Provider, id: &str) {
    assert!(
        EntityRepo::find_by_gts_id(&db.conn().expect("conn"), &allow_all(), id)
            .await
            .expect("entity lookup")
            .is_none()
    );
}

#[tokio::test]
async fn closure_counts_the_candidate_and_deduplicates_ref_and_derivation_targets() {
    let db = test_db().await;
    let mut limits = Limits {
        resolution_closure: 1,
        ..Limits::default()
    };
    let base = admit_with(
        &db,
        &limits,
        &common::worker_settings(),
        "base",
        BASE,
        base_schema("name"),
        None,
    )
    .await;
    succeeded(&base);
    // DERIVED reaches BASE through both its identifier and an authored $ref.
    let rejected = admit_with(
        &db,
        &limits,
        &common::worker_settings(),
        "over",
        DERIVED,
        derived_schema(),
        None,
    )
    .await;
    refused(
        &rejected,
        &AdmissionFailureReason::ResolutionClosureExceeded,
    );
    no_entity(&db, DERIVED).await;
    limits.resolution_closure = 2;
    let accepted = admit_with(
        &db,
        &limits,
        &common::worker_settings(),
        "boundary",
        DERIVED,
        derived_schema(),
        None,
    )
    .await;
    succeeded(&accepted);
}

#[tokio::test]
async fn closure_counts_transitive_documents_and_ignores_x_gts_ref() {
    let db = test_db().await;
    admit(&db, "base", BASE, base_schema("name"), None).await;
    admit(
        &db,
        "referrer",
        REFERRER,
        referencing_schema(REFERRER),
        None,
    )
    .await;
    let mut content = referencing_schema(SECOND);
    content["properties"]["subject"]["$ref"] = json!(format!("gts://{REFERRER}"));
    content["properties"]["identifier"] = json!({"type": "string", "x-gts-ref": INSTANCE});
    let mut limits = Limits {
        resolution_closure: 2,
        ..Limits::default()
    };
    let rejected = admit_with(
        &db,
        &limits,
        &common::worker_settings(),
        "over",
        SECOND,
        content.clone(),
        None,
    )
    .await;
    refused(
        &rejected,
        &AdmissionFailureReason::ResolutionClosureExceeded,
    );
    no_entity(&db, SECOND).await;
    limits.resolution_closure = 3;
    let accepted = admit_with(
        &db,
        &limits,
        &common::worker_settings(),
        "boundary",
        SECOND,
        content,
        None,
    )
    .await;
    succeeded(&accepted);
}

#[tokio::test]
async fn closure_uses_the_revision_overlay_instead_of_its_old_outgoing_edges() {
    let db = test_db().await;
    admit(&db, "base", BASE, base_schema("name"), None).await;
    admit(
        &db,
        "referrer",
        REFERRER,
        referencing_schema(REFERRER),
        None,
    )
    .await;
    let mut replacement = referencing_schema(REFERRER);
    replacement["properties"] = json!({});
    let limits = Limits {
        resolution_closure: 1,
        ..Limits::default()
    };
    let outcome = admit_with(
        &db,
        &limits,
        &common::worker_settings(),
        "drop-ref",
        REFERRER,
        replacement,
        Some(1),
    )
    .await;
    assert_eq!(succeeded(&outcome).resource_version, Some(2));
}

#[tokio::test]
async fn refresh_budgets_each_document_instead_of_the_shared_store() {
    let db = test_db().await;
    seed_base_and_dependents(&db).await;
    let limits = Limits {
        resolution_closure: 2,
        ..Limits::default()
    };
    let outcome = admit_with(
        &db,
        &limits,
        &common::worker_settings(),
        "revise",
        BASE,
        base_schema("label"),
        Some(1),
    )
    .await;
    succeeded(&outcome);
    // The shared store has BASE + DERIVED + REFERRER, but each closure has at most two.
    for id in [DERIVED, REFERRER] {
        assert!(current(&db, id).await.resolved_schema.contains("label"));
    }
}

async fn assert_revision_rolled_back(
    db: &Provider,
    before_base: CurrentTypeSchemaRow,
    before_referrer: CurrentTypeSchemaRow,
) {
    assert_eq!(entity(db, BASE).await.resource_version, 1);
    assert_eq!(current(db, BASE).await, before_base);
    assert_eq!(current(db, REFERRER).await, before_referrer);
    assert!(
        type_schema_revision::Entity::find_by_id((entity(db, BASE).await.id, 2))
            .secure()
            .scope_with(&allow_all())
            .one(&db.conn().expect("conn"))
            .await
            .expect("revision lookup")
            .is_none()
    );
}

#[tokio::test]
async fn a_dependent_closure_over_budget_rolls_back_the_candidate_revision() {
    let db = test_db().await;
    admit(&db, "base", BASE, base_schema("name"), None).await;
    admit(
        &db,
        "referrer",
        REFERRER,
        referencing_schema(REFERRER),
        None,
    )
    .await;
    let mut extra = base_schema("extra");
    extra["$id"] = json!(format!("gts://{SECOND}"));
    admit(&db, "extra", SECOND, extra, None).await;
    let before_base = current(&db, BASE).await;
    let before_referrer = current(&db, REFERRER).await;
    let mut replacement = base_schema("name");
    replacement["properties"]["extra"] = json!({"$ref": format!("gts://{SECOND}")});
    let limits = Limits {
        resolution_closure: 2,
        ..Limits::default()
    };
    let outcome = admit_with(
        &db,
        &limits,
        &common::worker_settings(),
        "revise",
        BASE,
        replacement,
        Some(1),
    )
    .await;
    refused(&outcome, &AdmissionFailureReason::ResolutionClosureExceeded);
    assert_revision_rolled_back(&db, before_base, before_referrer).await;
}

#[tokio::test]
async fn resolved_document_counts_utf8_bytes_and_accepts_the_exact_boundary() {
    let db = test_db().await;
    let content = base_schema("\u{1f980}");
    let size = canonical_text(&content).len();
    assert!(size > canonical_text(&content).chars().count());
    let mut limits = Limits {
        resolved_document: ByteSize::from_bytes(size - 1),
        ..Limits::default()
    };
    let rejected = admit_with(
        &db,
        &limits,
        &common::worker_settings(),
        "over",
        BASE,
        content.clone(),
        None,
    )
    .await;
    refused(&rejected, &AdmissionFailureReason::ResolvedDocumentTooLarge);
    no_entity(&db, BASE).await;
    limits.resolved_document = ByteSize::from_bytes(size);
    let accepted = admit_with(
        &db,
        &limits,
        &common::worker_settings(),
        "boundary",
        BASE,
        content,
        None,
    )
    .await;
    succeeded(&accepted);
    assert_eq!(current(&db, BASE).await.resolved_schema.len(), size);
}

#[tokio::test]
async fn an_expanded_dependent_over_budget_rolls_back_the_candidate_revision() {
    let db = test_db().await;
    admit(&db, "base", BASE, base_schema("name"), None).await;
    admit(
        &db,
        "referrer",
        REFERRER,
        referencing_schema(REFERRER),
        None,
    )
    .await;
    let before_base = current(&db, BASE).await;
    let before_referrer = current(&db, REFERRER).await;
    let limit = before_referrer.resolved_schema.len();
    let replacement = base_schema("longer_name");
    assert!(
        canonical_text(&replacement).len() <= limit,
        "candidate itself must fit"
    );
    let limits = Limits {
        resolved_document: ByteSize::from_bytes(limit),
        ..Limits::default()
    };
    let outcome = admit_with(
        &db,
        &limits,
        &common::worker_settings(),
        "revise",
        BASE,
        replacement,
        Some(1),
    )
    .await;
    refused(&outcome, &AdmissionFailureReason::ResolvedDocumentTooLarge);
    assert_revision_rolled_back(&db, before_base, before_referrer).await;
}

#[tokio::test]
async fn an_instance_counts_its_conforming_type_and_obeys_its_resolved_size_budget() {
    let db = test_db().await;
    admit(&db, "base", BASE, base_schema("name"), None).await;
    let size = current(&db, BASE).await.resolved_schema.len();
    for (key, limits, reason) in [
        (
            "closure",
            Limits {
                resolution_closure: 1,
                ..Limits::default()
            },
            AdmissionFailureReason::ResolutionClosureExceeded,
        ),
        (
            "size",
            Limits {
                resolved_document: ByteSize::from_bytes(size - 1),
                ..Limits::default()
            },
            AdmissionFailureReason::ResolvedDocumentTooLarge,
        ),
    ] {
        let outcome = admit_with(
            &db,
            &limits,
            &common::worker_settings(),
            key,
            INSTANCE,
            json!({"name": "first"}),
            None,
        )
        .await;
        refused(&outcome, &reason);
        no_entity(&db, INSTANCE).await;
    }
    let limits = Limits {
        resolution_closure: 2,
        resolved_document: ByteSize::from_bytes(size),
        ..Limits::default()
    };
    let outcome = admit_with(
        &db,
        &limits,
        &common::worker_settings(),
        "boundary",
        INSTANCE,
        json!({"name": "first"}),
        None,
    )
    .await;
    succeeded(&outcome);
}
