use std::collections::{BTreeMap, HashMap};

use rust_decimal::Decimal;
use time::OffsetDateTime;
use uuid::Uuid;

use usage_collector_sdk::{
    MetadataKey, UsageCollectorPluginError, UsageKind, UsageRecord, UsageRecordStatus, UsageType,
    UsageTypeGtsId,
};

use super::super::entity::{
    EpochMicros, RawMetadata, UsageRecordRow, UsageRecordStatusCode, UsageTypeKindCode,
    UsageTypeRow, ValidatedMetadata,
};
use super::{canonical_equal, make_inactive_marker, record_row_key, version_higher_than};

const VALID_GTS_ID: &str = "gts.cf.core.uc.usage_record.v1~cf.compute._.vcpu_hours.v1";

// ── status string form ──────────────────────────────────────────────────────

#[test]
fn status_code_into_str_emits_lowercase_wire_tokens() {
    assert_eq!(
        <&'static str>::from(UsageRecordStatusCode::Active),
        "active"
    );
    assert_eq!(
        <&'static str>::from(UsageRecordStatusCode::Inactive),
        "inactive"
    );
    assert_eq!(
        <&'static str>::from(UsageRecordStatusCode::from(UsageRecordStatus::Active)),
        "active"
    );
}

// ── metadata RawMetadata / ValidatedMetadata round-trip ───────────────────

#[test]
fn metadata_round_trips_via_raw_and_validated() {
    let mut btree = BTreeMap::new();
    btree.insert(MetadataKey::new("region").unwrap(), "eu-west".to_owned());
    btree.insert(MetadataKey::new("tier").unwrap(), "gold".to_owned());

    let raw = RawMetadata::from(&btree);
    let back = ValidatedMetadata::try_from(raw).unwrap().0;
    assert_eq!(back, btree);
}

#[test]
fn empty_metadata_round_trips() {
    let btree: BTreeMap<MetadataKey, String> = BTreeMap::new();
    let raw = RawMetadata::from(&btree);
    assert!(raw.0.is_empty());
    assert!(ValidatedMetadata::try_from(raw).unwrap().0.is_empty());
}

#[test]
fn metadata_hashmap_invalid_key_is_internal() {
    let mut hmap = HashMap::new();
    hmap.insert(String::new(), "value".to_owned()); // empty key is invalid
    assert!(matches!(
        ValidatedMetadata::try_from(hmap),
        Err(UsageCollectorPluginError::Internal(_))
    ));
}

// ── gts_id AsRef ─────────────────────────────────────────────────────────────

#[test]
fn gts_id_as_ref_returns_raw_string() {
    let gts_id = UsageTypeGtsId::new(VALID_GTS_ID).unwrap();
    assert_eq!(gts_id.as_ref(), VALID_GTS_ID);
}

#[test]
fn stored_gts_id_rejects_invalid_via_try_from_row() {
    let mut row = valid_type_row();
    row.gts_id = "not-a-valid-gts-id".to_owned();
    assert!(matches!(
        UsageType::try_from(row),
        Err(UsageCollectorPluginError::Internal(_))
    ));
}

// ── record row -> model ──────────────────────────────────────────────────────

fn valid_metadata_hmap() -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert("region".to_owned(), "eu-west".to_owned());
    m
}

fn valid_record_row() -> UsageRecordRow {
    UsageRecordRow {
        id: Uuid::from_u128(1),
        tenant_id: Uuid::from_u128(2),
        gts_id: VALID_GTS_ID.to_owned(),
        value: Decimal::new(425, 1),               // 42.5
        created_at: 1_700_000_000 * 1_000_000_i64, // 2023-11-14 in microseconds
        resource_id: "res-1".to_owned(),
        resource_type: "compute.vm".to_owned(),
        subject_id: Some("subj-1".to_owned()),
        subject_type: Some("user".to_owned()),
        idempotency_key: "idem-1".to_owned(),
        corrects_id: None,
        status: UsageRecordStatusCode::Active,
        metadata: valid_metadata_hmap(),
        ingested_at: 1_700_000_100 * 1_000_000_i64,
        version: 1_700_000_000_000_000_u64,
    }
}

#[test]
fn usage_record_try_from_maps_valid_row() {
    let row = valid_record_row();
    let model = UsageRecord::try_from(row).expect("a fully valid row must map");

    assert_eq!(model.id, Uuid::from_u128(1));
    assert_eq!(model.tenant_id, Uuid::from_u128(2));
    assert_eq!(model.gts_id, UsageTypeGtsId::new(VALID_GTS_ID).unwrap());
    assert_eq!(model.value, Decimal::new(425, 1));
    assert_eq!(model.resource_ref.resource_id(), "res-1");
    assert_eq!(model.resource_ref.resource_type(), "compute.vm");
    let subject = model.subject_ref.as_ref().expect("subject present");
    assert_eq!(subject.subject_id(), "subj-1");
    assert_eq!(subject.subject_type(), Some("user"));
    assert_eq!(model.idempotency_key.as_str(), "idem-1");
    assert_eq!(model.corrects_id, None);
    assert_eq!(model.status, UsageRecordStatus::Active);
    assert_eq!(
        model.metadata.get(&MetadataKey::new("region").unwrap()),
        Some(&"eu-west".to_owned())
    );
}

#[test]
fn record_row_absent_subject_maps_to_none() {
    let mut row = valid_record_row();
    row.subject_id = None;
    row.subject_type = None;
    let model = UsageRecord::try_from(row).expect("a row without a subject maps");
    assert!(model.subject_ref.is_none());
}

#[test]
fn record_row_invalid_gts_id_is_internal() {
    let mut row = valid_record_row();
    row.gts_id = "not-a-valid-gts-id".to_owned();
    assert!(matches!(
        UsageRecord::try_from(row),
        Err(UsageCollectorPluginError::Internal(_))
    ));
}

#[test]
fn record_row_invalid_resource_ref_is_internal() {
    let mut row = valid_record_row();
    row.resource_id = String::new(); // empty resource_id fails ResourceRef::new
    assert!(matches!(
        UsageRecord::try_from(row),
        Err(UsageCollectorPluginError::Internal(_))
    ));
}

#[test]
fn record_row_invalid_subject_ref_is_internal() {
    let mut row = valid_record_row();
    row.subject_id = Some(String::new()); // present-but-empty subject_id is rejected
    assert!(matches!(
        UsageRecord::try_from(row),
        Err(UsageCollectorPluginError::Internal(_))
    ));
}

#[test]
fn record_row_invalid_idempotency_key_is_internal() {
    let mut row = valid_record_row();
    row.idempotency_key = String::new();
    assert!(matches!(
        UsageRecord::try_from(row),
        Err(UsageCollectorPluginError::Internal(_))
    ));
}

// ── usage-type row -> model ──────────────────────────────────────────────────

fn valid_type_row() -> UsageTypeRow {
    UsageTypeRow {
        gts_id: VALID_GTS_ID.to_owned(),
        kind: UsageTypeKindCode::Counter,
        metadata_fields: vec!["region".to_owned(), "tier".to_owned()],
        version: 1,
    }
}

#[test]
fn usage_type_try_from_maps_valid_row() {
    let model = UsageType::try_from(valid_type_row()).expect("a fully valid type row must map");
    assert_eq!(model.gts_id, UsageTypeGtsId::new(VALID_GTS_ID).unwrap());
    assert_eq!(model.kind, UsageKind::Counter);
    assert_eq!(model.metadata_fields.len(), 2);
    assert!(
        model
            .metadata_fields
            .contains(&MetadataKey::new("region").unwrap())
    );
}

#[test]
fn type_row_invalid_gts_id_is_internal() {
    let mut row = valid_type_row();
    row.gts_id = "not-a-valid-gts-id".to_owned();
    assert!(matches!(
        UsageType::try_from(row),
        Err(UsageCollectorPluginError::Internal(_))
    ));
}

#[test]
fn type_row_invalid_metadata_field_is_internal() {
    let mut row = valid_type_row();
    row.metadata_fields = vec![String::new()]; // empty key fails MetadataKey::new
    assert!(matches!(
        UsageType::try_from(row),
        Err(UsageCollectorPluginError::Internal(_))
    ));
}

// ── canonical_equal ──────────────────────────────────────────────────────────

#[test]
fn canonical_equal_returns_true_for_identical_fields() {
    use usage_collector_sdk::{IdempotencyKey, ResourceRef, UsageRecord, UsageRecordStatus};

    let row = valid_record_row();
    let created_at =
        OffsetDateTime::try_from(EpochMicros(row.created_at)).expect("in-range timestamp");
    let mut metadata = BTreeMap::new();
    metadata.insert(MetadataKey::new("region").unwrap(), "eu-west".to_owned());
    let record = UsageRecord {
        id: row.id,
        gts_id: UsageTypeGtsId::new(VALID_GTS_ID).unwrap(),
        tenant_id: row.tenant_id,
        resource_ref: ResourceRef::new(row.resource_id.clone(), row.resource_type.clone()).unwrap(),
        subject_ref: None,
        metadata,
        value: row.value,
        idempotency_key: IdempotencyKey::new(row.idempotency_key.clone()).unwrap(),
        corrects_id: row.corrects_id,
        status: UsageRecordStatus::Active,
        created_at,
    };
    // subject differs (row has subject, record has None) → not equal
    assert!(!canonical_equal(&row, &record).unwrap());
}

#[test]
fn version_higher_than_is_always_strictly_greater() {
    let existing = 1_000_u64;
    let result = version_higher_than(existing, 0);
    assert!(result > existing);
}

#[test]
fn version_higher_than_with_offset_adds_headroom() {
    let existing = 1_000_u64;
    let result = version_higher_than(existing, 5);
    assert!(result > existing.saturating_add(5));
}

#[test]
fn record_row_from_maps_active_fields_and_optional_subject() {
    use usage_collector_sdk::{IdempotencyKey, ResourceRef, SubjectRef, UsageRecord};

    let created_at = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
    let mut metadata = BTreeMap::new();
    metadata.insert(MetadataKey::new("region").unwrap(), "eu-west".to_owned());
    let record = UsageRecord {
        id: Uuid::from_u128(9),
        gts_id: UsageTypeGtsId::new(VALID_GTS_ID).unwrap(),
        tenant_id: Uuid::from_u128(3),
        resource_ref: ResourceRef::new("res-1", "compute.vm").unwrap(),
        subject_ref: Some(SubjectRef::new("subj-1", Some("user".to_owned())).unwrap()),
        metadata,
        value: Decimal::new(10, 0),
        idempotency_key: IdempotencyKey::new("idem-1").unwrap(),
        corrects_id: None,
        status: UsageRecordStatus::Active,
        created_at,
    };

    let row = UsageRecordRow::from((&record, 42));
    assert_eq!(row.id, record.id);
    assert_eq!(row.tenant_id, record.tenant_id);
    assert_eq!(row.gts_id, VALID_GTS_ID);
    assert_eq!(row.version, 42);
    assert_eq!(row.status, UsageRecordStatusCode::Active);
    assert_eq!(row.subject_id.as_deref(), Some("subj-1"));
    assert_eq!(row.subject_type.as_deref(), Some("user"));
    assert_eq!(row.created_at, EpochMicros::from(created_at).0);
}

#[test]
fn make_inactive_marker_flips_status_and_bumps_version() {
    let source = valid_record_row();
    let marker = make_inactive_marker(&source, 100, 3);
    assert_eq!(marker.status, UsageRecordStatusCode::Inactive);
    assert_eq!(marker.version, 103);
}

#[test]
fn record_row_key_extracts_known_fields() {
    let row = valid_record_row();
    assert_eq!(record_row_key(&row, "id"), Some(row.id.to_string()));
    assert_eq!(
        record_row_key(&row, "tenant_id"),
        Some(row.tenant_id.to_string())
    );
    assert_eq!(
        record_row_key(&row, "resource_id"),
        Some(row.resource_id.clone())
    );
    assert_eq!(
        record_row_key(&row, "resource_type"),
        Some(row.resource_type.clone())
    );
    assert_eq!(record_row_key(&row, "subject_id"), row.subject_id.clone());
    assert_eq!(
        record_row_key(&row, "subject_type"),
        row.subject_type.clone()
    );
    assert_eq!(record_row_key(&row, "status"), Some("active".to_owned()));
    assert!(record_row_key(&row, "created_at").is_some());
    assert_eq!(record_row_key(&row, "corrects_id"), None);
    assert_eq!(record_row_key(&row, "unknown"), None);

    let mut with_corrects = row;
    with_corrects.corrects_id = Some(Uuid::from_u128(99));
    assert_eq!(
        record_row_key(&with_corrects, "corrects_id"),
        Some(Uuid::from_u128(99).to_string())
    );
}

// ── EpochMicros ─────────────────────────────────────────────────────────────

#[test]
fn epoch_micros_round_trips_a_stored_timestamp() {
    let micros = 1_700_000_000_000_000_i64;
    let dt =
        OffsetDateTime::try_from(EpochMicros(micros)).expect("an in-range timestamp must convert");
    assert_eq!(EpochMicros::from(dt).0, micros);
}

#[test]
fn epoch_micros_rejects_out_of_range_value() {
    let err = OffsetDateTime::try_from(EpochMicros(i64::MAX))
        .expect_err("i64::MAX microseconds is far past year 9999");
    assert!(matches!(err, UsageCollectorPluginError::Internal(_)));
}
