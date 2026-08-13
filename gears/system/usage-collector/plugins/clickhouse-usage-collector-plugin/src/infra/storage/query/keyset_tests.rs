use toolkit_odata::filter::{FieldKind, FilterField};
use toolkit_odata::{CursorV1, ODataOrderBy, OrderKey, SortDir};
use usage_collector_sdk::UsageRecordFilterField;

use super::super::bind::SqlBind;
use super::super::translate::{SqlCtx, record_column};
use super::{
    cursor_key_to_bind, encode_next_cursor, ensure_forward_cursor, keyset_predicate,
    render_order_by,
};

fn rec_kind(name: &str) -> Option<FieldKind> {
    <UsageRecordFilterField as FilterField>::from_name(name).map(|f| f.kind())
}

fn rec_keyset_safe(name: &str) -> bool {
    usage_collector_sdk::is_keyset_safe_record_field(name)
}

fn order_created_at_id() -> ODataOrderBy {
    ODataOrderBy(vec![
        OrderKey {
            field: "created_at".to_owned(),
            dir: SortDir::Asc,
        },
        OrderKey {
            field: "id".to_owned(),
            dir: SortDir::Asc,
        },
    ])
}

fn order_created_at_id_desc() -> ODataOrderBy {
    ODataOrderBy(vec![
        OrderKey {
            field: "created_at".to_owned(),
            dir: SortDir::Desc,
        },
        OrderKey {
            field: "id".to_owned(),
            dir: SortDir::Desc,
        },
    ])
}

#[test]
fn datetime_keyset_uses_epoch_microsecond_conversion() {
    let mut ctx = SqlCtx::new();

    let predicate = keyset_predicate(
        &[("created_at", true), ("id", true)],
        &[
            "2026-08-10T11:00:00Z".to_owned(),
            "00000000-0000-4000-8000-000000000001".to_owned(),
        ],
        |field| match field {
            "created_at" => Some("created_at"),
            "id" => Some("id"),
            _ => None,
        },
        |field| match field {
            "created_at" => Some(FieldKind::DateTimeUtc),
            "id" => Some(FieldKind::Uuid),
            _ => None,
        },
        |_| true,
        &mut ctx,
    )
    .unwrap();

    assert_eq!(
        predicate,
        "(created_at, id) > (fromUnixTimestamp64Micro(?), ?)"
    );
    assert_eq!(ctx.binds.len(), 2);
}

#[test]
fn ensure_forward_cursor_rejects_backward_direction() {
    let mk = |d: &str| CursorV1 {
        k: vec!["x".to_owned()],
        o: SortDir::Asc,
        s: "+created_at".to_owned(),
        f: None,
        d: d.to_owned(),
    };
    assert!(
        ensure_forward_cursor(&mk("fwd")).is_ok(),
        "a forward cursor is accepted"
    );
    assert!(
        ensure_forward_cursor(&mk("bwd")).is_err(),
        "a backward cursor is rejected"
    );
}

#[test]
fn render_order_by_renders_allowlisted_columns() {
    let sql = render_order_by(&order_created_at_id(), record_column).unwrap();
    assert_eq!(sql, "created_at ASC, id ASC");

    let desc = render_order_by(&order_created_at_id_desc(), record_column).unwrap();
    assert_eq!(desc, "created_at DESC, id DESC");
}

#[test]
fn render_order_by_rejects_unknown_column() {
    let order = ODataOrderBy(vec![OrderKey {
        field: "not_a_column".to_owned(),
        dir: SortDir::Asc,
    }]);
    assert!(render_order_by(&order, record_column).is_err());
}

#[test]
fn render_order_by_rejects_empty_order() {
    let err = render_order_by(&ODataOrderBy(vec![]), record_column).unwrap_err();
    assert!(err.contains("order must not be empty"), "got: {err}");
}

#[test]
fn keyset_predicate_rejects_empty_order_pairs() {
    let pairs: &[(&str, bool)] = &[];
    let keys: Vec<String> = vec![];
    let mut ctx = SqlCtx::new();
    let err = keyset_predicate(
        pairs,
        &keys,
        record_column,
        rec_kind,
        rec_keyset_safe,
        &mut ctx,
    )
    .unwrap_err();
    assert!(err.contains("keyset order must not be empty"), "got: {err}");
}

#[test]
fn keyset_predicate_rejects_key_order_arity_mismatch() {
    let pairs: &[(&str, bool)] = &[("created_at", true), ("id", true)];
    let keys = vec!["2026-01-02T03:04:05Z".to_owned()];
    let mut ctx = SqlCtx::new();
    let err = keyset_predicate(
        pairs,
        &keys,
        record_column,
        rec_kind,
        rec_keyset_safe,
        &mut ctx,
    )
    .unwrap_err();
    assert!(err.contains("does not match order arity"), "got: {err}");
}

#[test]
fn keyset_predicate_descending_uses_less_than() {
    let pairs: &[(&str, bool)] = &[("created_at", false), ("id", false)];
    let keys = vec![
        "2026-01-02T03:04:05Z".to_owned(),
        uuid::Uuid::from_u128(0x1234).to_string(),
    ];
    let mut ctx = SqlCtx::new();
    let sql = keyset_predicate(
        pairs,
        &keys,
        record_column,
        rec_kind,
        rec_keyset_safe,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(sql, "(created_at, id) < (fromUnixTimestamp64Micro(?), ?)");
}

#[test]
fn keyset_predicate_rejects_mixed_directions() {
    let pairs: &[(&str, bool)] = &[("created_at", true), ("id", false)];
    let keys = vec!["2026-01-02T03:04:05Z".to_owned(), "x".to_owned()];
    let mut ctx = SqlCtx::new();
    assert!(
        keyset_predicate(
            pairs,
            &keys,
            record_column,
            rec_kind,
            rec_keyset_safe,
            &mut ctx
        )
        .is_err()
    );
}

#[test]
fn keyset_predicate_rejects_a_nullable_ordering_column() {
    let pairs: &[(&str, bool)] = &[("subject_type", true)];
    let keys = vec!["vm".to_owned()];
    let mut ctx = SqlCtx::new();
    let err = keyset_predicate(
        pairs,
        &keys,
        record_column,
        rec_kind,
        rec_keyset_safe,
        &mut ctx,
    )
    .unwrap_err();
    assert!(
        err.contains("nullable") && err.contains("subject_type"),
        "error must name the nullable offending field; got: {err}"
    );
    assert!(ctx.binds.is_empty(), "no bind is pushed on the reject path");
}

#[test]
fn keyset_predicate_rejects_unknown_allowlist_field() {
    let pairs: &[(&str, bool)] = &[("not_a_column", true)];
    let keys = vec!["x".to_owned()];
    let mut ctx = SqlCtx::new();
    let err =
        keyset_predicate(pairs, &keys, record_column, |_| None, |_| true, &mut ctx).unwrap_err();
    assert!(err.contains("not allowlisted"), "got: {err}");
}

#[test]
fn cursor_key_to_bind_dispatches_on_field_kind() {
    assert!(matches!(
        cursor_key_to_bind(FieldKind::DateTimeUtc, "2026-01-02T03:04:05Z").unwrap(),
        SqlBind::DateTime64Micros(_)
    ));
    assert!(matches!(
        cursor_key_to_bind(FieldKind::Uuid, &uuid::Uuid::from_u128(1).to_string()).unwrap(),
        SqlBind::Uuid(_)
    ));
    assert!(matches!(
        cursor_key_to_bind(FieldKind::String, "active").unwrap(),
        SqlBind::Str(_)
    ));
    assert!(cursor_key_to_bind(FieldKind::DateTimeUtc, "not-a-date").is_err());
    assert!(cursor_key_to_bind(FieldKind::Uuid, "not-a-uuid").is_err());
    assert!(cursor_key_to_bind(FieldKind::Decimal, "1.5").is_err());
    assert!(cursor_key_to_bind(FieldKind::I64, "5").is_err());
}

#[test]
fn encode_then_decode_cursor_round_trips_keys_and_order() {
    let order = order_created_at_id();
    let keys = vec![
        "2026-01-02T03:04:05Z".to_owned(),
        uuid::Uuid::from_u128(0x1234).to_string(),
    ];
    let token = encode_next_cursor(&order, &keys, Some("hash")).unwrap();
    let decoded = CursorV1::decode(&token).unwrap();
    assert_eq!(decoded.k, keys);
    assert_eq!(decoded.s, "+created_at,+id");
    assert_eq!(decoded.d, "fwd");
    assert_eq!(decoded.f.as_deref(), Some("hash"));
}

#[test]
fn encode_next_cursor_rejects_row_key_order_arity_mismatch() {
    let order = order_created_at_id();
    let keys = vec!["2026-01-02T03:04:05Z".to_owned()];
    let err = encode_next_cursor(&order, &keys, None).unwrap_err();
    assert!(err.contains("does not match order arity"), "got: {err}");
}

#[test]
fn encode_next_cursor_rejects_empty_order() {
    let err = encode_next_cursor(&ODataOrderBy(vec![]), &[], None).unwrap_err();
    assert!(err.contains("cursor order must not be empty"), "got: {err}");
}
