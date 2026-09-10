#![allow(clippy::expect_used)]

use gts::GtsTypeId;
use quota_enforcement_sdk::{
    CapPatch, ContractRef, EnforcementMode, PeriodType, Quota, QuotaId, QuotaPatch, QuotaStatus,
    QuotaType, ValidityWindow, ValidityWindowPatch,
};
use serde_json::json;
use time::OffsetDateTime;
use time::macros::datetime;

use super::{
    MappingError, QuotaUpdate, STATUS_ACTIVE, draft_to_row, patch_to_update, row_to_quota,
    thresholds_of,
};
use crate::infra::storage::entity::quota;
use crate::test_support::{draft, tenant};

fn now() -> OffsetDateTime {
    datetime!(2026-09-09 12:00:00 UTC)
}

fn stored(cap: Option<u64>) -> (QuotaId, quota::Model) {
    let mut draft = draft(tenant(), "u1", cap);
    draft.quota_type = QuotaType::Consumption;
    draft.period = Some(PeriodType::Month);
    draft.notification_thresholds = vec![50, 90];
    draft.validity_window = Some(ValidityWindow {
        start: Some(datetime!(2026-01-01 00:00:00 UTC)),
        end: None,
    });
    draft.metadata = json!({ "regions": ["eu"], "weight": 5 })
        .as_object()
        .cloned()
        .expect("object");
    let id = QuotaId::generate();
    let row = draft_to_row(id, &draft, now()).expect("fits");
    let model = quota::Model {
        id: row.id.unwrap(),
        tenant_id: row.tenant_id.unwrap(),
        projection_type: row.projection_type.unwrap(),
        subject_id: row.subject_id.unwrap(),
        metric: row.metric.unwrap(),
        quota_type: row.quota_type.unwrap(),
        period: row.period.unwrap(),
        enforcement_mode: row.enforcement_mode.unwrap(),
        cap: row.cap.unwrap(),
        notification_thresholds: row.notification_thresholds.unwrap(),
        validity_start: row.validity_start.unwrap(),
        validity_end: row.validity_end.unwrap(),
        fail_open_hint: row.fail_open_hint.unwrap(),
        metadata: row.metadata.unwrap(),
        source: row.source.unwrap(),
        status: row.status.unwrap(),
        constraint_contract_type: row.constraint_contract_type.unwrap(),
        constraint_contract_version: row.constraint_contract_version.unwrap(),
        record_version: row.record_version.unwrap(),
        created_at: row.created_at.unwrap(),
        updated_at: row.updated_at.unwrap(),
    };
    (id, model)
}

#[test]
fn a_draft_round_trips_through_the_row() {
    let (id, model) = stored(Some(100));
    assert_eq!(model.status, STATUS_ACTIVE);
    assert_eq!(model.record_version, 1);
    assert_eq!(model.notification_thresholds, "[50,90]");
    let quota: Quota = row_to_quota(model).expect("reads back");
    assert_eq!(quota.id, id);
    assert_eq!(quota.tenant_id, tenant());
    assert_eq!(quota.quota_type, QuotaType::Consumption);
    assert_eq!(quota.period, Some(PeriodType::Month));
    assert_eq!(quota.enforcement_mode, EnforcementMode::Hard);
    assert_eq!(quota.cap, Some(100));
    assert_eq!(quota.notification_thresholds, vec![50, 90]);
    assert_eq!(
        quota.validity_window,
        Some(ValidityWindow {
            start: Some(datetime!(2026-01-01 00:00:00 UTC)),
            end: None,
        })
    );
    assert_eq!(quota.metadata["weight"], json!(5));
    assert_eq!(quota.status, QuotaStatus::Active);
    assert_eq!(quota.record_version, 1);
    assert_eq!(quota.constraint_contract.version, 1);
    assert_eq!(quota.created_at, now());
}

#[test]
fn an_unbounded_cap_and_an_empty_window_read_back_as_none() {
    let (_, mut model) = stored(None);
    model.validity_start = None;
    model.validity_end = None;
    let quota = row_to_quota(model).expect("reads back");
    assert_eq!(quota.cap, None);
    assert_eq!(quota.validity_window, None);
}

#[test]
fn caller_values_that_do_not_fit_their_columns_are_refused_before_the_write() {
    let mut over = draft(tenant(), "u1", Some(u64::MAX));
    assert_eq!(
        draft_to_row(QuotaId::generate(), &over, now()).expect_err("cap"),
        MappingError::CapOutOfRange { cap: u64::MAX }
    );
    over.cap = Some(Quota::MAX_CAP);
    over.constraint_contract.version = u32::MAX;
    assert_eq!(
        draft_to_row(QuotaId::generate(), &over, now()).expect_err("version"),
        MappingError::VersionOutOfRange {
            field: "constraint_contract.version",
            value: u64::from(u32::MAX),
        }
    );
    assert_eq!(
        patch_to_update(&QuotaPatch {
            cap: Some(CapPatch::Bounded(u64::MAX)),
            ..QuotaPatch::default()
        })
        .expect_err("cap"),
        MappingError::CapOutOfRange { cap: u64::MAX }
    );
}

#[test]
fn a_row_that_does_not_read_as_the_contract_type_names_its_column() {
    type Corrupt = fn(&mut quota::Model);
    let cases: [(&str, Corrupt); 6] = [
        ("status", |m| m.status = "paused".to_owned()),
        ("cap", |m| m.cap = Some(-1)),
        ("quota_type", |m| m.quota_type = "rate".to_owned()),
        ("period", |m| m.period = Some("weekly".to_owned())),
        ("notification_thresholds", |m| {
            m.notification_thresholds = "[500]".to_owned();
        }),
        ("metadata", |m| m.metadata = "[]".to_owned()),
    ];
    for (column, corrupt) in cases {
        let (_, mut model) = stored(Some(1));
        corrupt(&mut model);
        match row_to_quota(model) {
            Err(MappingError::Column { column: got, .. }) => assert_eq!(got, column),
            other => panic!("{column}: {other:?}"),
        }
    }
}

#[test]
fn a_patch_maps_onto_column_changes_and_merges_over_the_current_row() {
    let contract_v2 = ContractRef {
        type_id: GtsTypeId::new(
            "gts.cf.core.qe.constraint.v1~cf.genai.llm_gateway.token_constraint.v2~",
        ),
        version: 2,
    };
    let update = patch_to_update(&QuotaPatch {
        cap: Some(CapPatch::Unbounded),
        notification_thresholds: Some(vec![]),
        validity_window: Some(ValidityWindowPatch::Clear),
        metadata: Some(json!({ "a": 1 }).as_object().cloned().expect("object")),
        constraint_contract: Some(contract_v2.clone()),
        enforcement_mode: Some(EnforcementMode::Hard),
        fail_open_hint: Some(true),
    })
    .expect("fits");
    assert_eq!(update.cap, Some(None));
    assert_eq!(update.notification_thresholds.as_deref(), Some("[]"));
    assert_eq!(update.validity, Some((None, None)));
    assert_eq!(update.metadata.as_deref(), Some(r#"{"a":1}"#));
    assert_eq!(
        update.constraint_contract,
        Some((contract_v2.type_id.as_ref().to_owned(), 2))
    );
    assert_eq!(
        patch_to_update(&QuotaPatch {
            metadata: Some(json!({ "a": 1 }).as_object().cloned().expect("object")),
            ..QuotaPatch::default()
        })
        .expect_err("metadata without its contract"),
        MappingError::MetadataWithoutContract
    );
    assert_eq!(
        patch_to_update(&QuotaPatch {
            fail_open_hint: Some(true),
            constraint_contract: Some(contract_v2),
            ..QuotaPatch::default()
        })
        .expect("a contract without metadata is ignored")
        .constraint_contract,
        None
    );
    assert_eq!(
        update.enforcement_mode.as_deref(),
        Some(EnforcementMode::Hard.as_gts_id())
    );
    assert_eq!(update.fail_open_hint, Some(true));

    let (_, model) = stored(Some(100));
    assert_eq!(update.merged_cap(&model), None, "the patch unbinds");
    assert_eq!(update.merged_thresholds(&model), "[]");
    let untouched = QuotaUpdate::default();
    assert_eq!(untouched.merged_cap(&model), Some(100));
    assert_eq!(
        thresholds_of(untouched.merged_thresholds(&model)).expect("json"),
        vec![50, 90]
    );
    assert_eq!(
        patch_to_update(&QuotaPatch {
            cap: Some(CapPatch::Bounded(7)),
            ..QuotaPatch::default()
        })
        .expect("fits")
        .cap,
        Some(Some(7))
    );
}
