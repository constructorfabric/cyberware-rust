//! Unit tests for the invoice-posting request DTOs: `snake_case` wire
//! deserialization + `into_domain` lowering (enum literals parsed at the
//! boundary; a bad literal is `DomainError::InvalidRequest` ⇒ HTTP 400).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::domain::instant::from_unix;

/// A valid `snake_case` `POST /journal-entries` body deserializes into the
/// request DTO and `into_domain` yields the expected `PostedInvoice`.
#[test]
fn post_invoice_body_deserializes_and_lowers_to_domain() {
    let tenant = uuid::uuid!("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
    let payer = uuid::uuid!("11111111-2222-3333-4444-555555555555");
    let actor = uuid::uuid!("99999999-8888-7777-6666-555555555555");
    let body = serde_json::json!({
        "tenant_id": tenant,
        "invoice_id": "INV-42",
        "payer_tenant_id": payer,
        "effective_at": "2026-06-01",
        "due_date": "2026-07-01",
        "period_id": "202606",
        "items": [
            {
                "amount_minor_ex_tax": 1000,
                "currency": "USD",
                "revenue_stream": "subscription",
                "catalog_class": "REVENUE",
                "gl_code": "4000"
            }
        ],
        "tax": [
            {
                "amount_minor": 200,
                "currency": "USD",
                "tax_jurisdiction": "US-CA",
                "tax_filing_period": "2026Q2"
            }
        ],
        "correlation_id": actor
    });

    let dto: PostInvoiceRequestDto =
        serde_json::from_value(body).expect("snake_case body must deserialize");
    // The poster identity is the authenticated subject, passed in by the handler
    // (never read from the body).
    let inv = dto
        .into_domain(actor)
        .expect("into_domain must lower cleanly");

    assert_eq!(inv.invoice_id, "INV-42");
    assert_eq!(inv.seller_tenant_id, tenant, "seller = body tenant_id");
    assert_eq!(
        inv.posted_by_actor_id, actor,
        "posted_by_actor_id is the authenticated subject, stamped server-side"
    );
    assert_eq!(inv.payer_tenant_id, payer);
    assert_eq!(inv.period_id, "202606");
    assert_eq!(inv.items.len(), 1);
    assert_eq!(inv.items[0].amount_minor_ex_tax, 1000);
    assert_eq!(inv.items[0].revenue_stream, "subscription");
    assert_eq!(
        inv.items[0].catalog_class,
        Some(AccountClass::Revenue),
        "catalog_class literal parsed at the boundary"
    );
    assert!(inv.items[0].contract_class.is_none());
    assert_eq!(inv.tax.len(), 1);
    assert_eq!(inv.tax[0].amount_minor, 200);
    assert_eq!(inv.tax[0].tax_jurisdiction, "US-CA");
    // Gross is derived downstream; assert the helper agrees (1000 + 200).
    assert_eq!(inv.gross_minor(), 1200);
}

/// A contract-class override is parsed and wins precedence at mapping time.
#[test]
fn invoice_item_contract_class_override_parses() {
    let dto = InvoiceItemDto {
        amount_minor_ex_tax: 500,
        currency: "USD".to_owned(),
        revenue_stream: "usage".to_owned(),
        catalog_class: Some("REVENUE".to_owned()),
        contract_class: Some("CONTRA_REVENUE".to_owned()),
        gl_code: None,
        recognition: None,
        invoice_item_ref: None,
        sku_or_plan_ref: None,
        price_id: None,
        pricing_snapshot_ref: None,
    };
    let item = dto.into_domain().expect("valid literals");
    assert_eq!(item.catalog_class, Some(AccountClass::Revenue));
    assert_eq!(item.contract_class, Some(AccountClass::ContraRevenue));
}

/// A missing mapping pair (both `None`) is valid at the DTO level — the line
/// routes to SUSPENSE/PENDING later, not a 400.
#[test]
fn invoice_item_without_mapping_is_valid() {
    let dto = InvoiceItemDto {
        amount_minor_ex_tax: 500,
        currency: "USD".to_owned(),
        revenue_stream: "usage".to_owned(),
        catalog_class: None,
        contract_class: None,
        gl_code: None,
        recognition: None,
        invoice_item_ref: None,
        sku_or_plan_ref: None,
        price_id: None,
        pricing_snapshot_ref: None,
    };
    let item = dto
        .into_domain()
        .expect("a missing mapping is not an error");
    assert!(item.catalog_class.is_none());
    assert!(item.contract_class.is_none());
}

/// A bad `catalog_class` literal lowers to `DomainError::InvalidRequest` (the
/// REST layer maps this to a 400, never a 500 or a silent default).
#[test]
fn invoice_item_bad_account_class_is_invalid_request() {
    let dto = InvoiceItemDto {
        amount_minor_ex_tax: 100,
        currency: "USD".to_owned(),
        revenue_stream: "subscription".to_owned(),
        catalog_class: Some("NOT_A_CLASS".to_owned()),
        contract_class: None,
        gl_code: None,
        recognition: None,
        invoice_item_ref: None,
        sku_or_plan_ref: None,
        price_id: None,
        pricing_snapshot_ref: None,
    };
    let err = dto
        .into_domain()
        .expect_err("a bad class literal must reject");
    assert!(
        matches!(err, DomainError::InvalidRequest(_)),
        "expected InvalidRequest, got {err:?}"
    );
}

/// Regression: a negative ex-tax amount is rejected at the boundary as
/// `InvalidRequest` (a 400) rather than reaching the recognition split's
/// `deferred.clamp(0, amount)` downstream, where `min > max` would panic.
#[test]
fn invoice_item_negative_amount_is_invalid_request() {
    let dto = InvoiceItemDto {
        amount_minor_ex_tax: -1,
        currency: "USD".to_owned(),
        revenue_stream: "subscription".to_owned(),
        catalog_class: None,
        contract_class: None,
        gl_code: None,
        recognition: None,
        invoice_item_ref: None,
        sku_or_plan_ref: None,
        price_id: None,
        pricing_snapshot_ref: None,
    };
    let err = dto
        .into_domain()
        .expect_err("a negative amount must reject");
    assert!(
        matches!(err, DomainError::InvalidRequest(_)),
        "expected InvalidRequest, got {err:?}"
    );
}

/// The mapping-correction body lowers its `corrected_items` the same way (bad
/// literal ⇒ `InvalidRequest`).
#[test]
fn mapping_correction_corrected_items_lower_to_domain() {
    let dto = MappingCorrectionRequestDto {
        reason: "wrong stream".to_owned(),
        period_id: None,
        effective_at: None,
        corrected_items: vec![InvoiceItemDto {
            amount_minor_ex_tax: 1000,
            currency: "USD".to_owned(),
            revenue_stream: "subscription".to_owned(),
            catalog_class: Some("REVENUE".to_owned()),
            contract_class: None,
            gl_code: None,
            recognition: None,
            invoice_item_ref: None,
            sku_or_plan_ref: None,
            price_id: None,
            pricing_snapshot_ref: None,
        }],
    };
    let items = dto.corrected_items_into_domain().expect("valid items");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].catalog_class, Some(AccountClass::Revenue));
}

// ── Slice 4: the optional recognition block ──────────────────────────────────

/// An item WITHOUT a `recognition` block lowers to `recognition: None` +
/// `deferred_minor: 0` — the unchanged Variant-A default.
#[test]
fn invoice_item_without_recognition_defaults_to_no_deferral() {
    let body = serde_json::json!({
        "amount_minor_ex_tax": 1000,
        "currency": "USD",
        "revenue_stream": "subscription",
        "catalog_class": "REVENUE"
    });
    let dto: InvoiceItemDto =
        serde_json::from_value(body).expect("snake_case item must deserialize");
    let item = dto.into_domain().expect("valid item");
    assert!(item.recognition.is_none(), "absent recognition ⇒ None");
    assert_eq!(
        item.deferred_minor, 0,
        "deferred is always seeded 0 at the DTO"
    );
}

/// A `straight_line` recognition block lowers to the domain timing (periods +
/// optional first period); the deferred amount is NOT taken from the wire.
#[test]
fn invoice_item_straight_line_recognition_lowers_to_domain() {
    let body = serde_json::json!({
        "amount_minor_ex_tax": 1200,
        "currency": "USD",
        "revenue_stream": "subscription",
        "catalog_class": "REVENUE",
        "recognition": {
            "policy_ref": "policy.sl.v1",
            "timing": "straight_line",
            "periods": 12,
            "po_allocation_group": "grp-1",
            "subscription_ref": "sub-1"
        }
    });
    let dto: InvoiceItemDto =
        serde_json::from_value(body).expect("snake_case item must deserialize");
    let item = dto.into_domain().expect("valid recognition block");
    let rec = item.recognition.expect("recognition present");
    assert_eq!(rec.policy_ref, "policy.sl.v1");
    assert!(
        matches!(
            rec.timing,
            RecognitionTiming::StraightLine {
                periods: 12,
                first_period_id: None
            }
        ),
        "straight_line lowers with periods=12, first_period defaulted later"
    );
    assert_eq!(rec.po_allocation_group.as_deref(), Some("grp-1"));
    assert!(!rec.multi_po, "multi_po defaults to false when omitted");
    assert_eq!(
        item.deferred_minor, 0,
        "the DTO never carries the deferred amount"
    );
}

/// A `point_in_time` recognition block lowers to `PointInTime` (an explicit
/// no-defer spec — distinct from absence, but same posting outcome).
#[test]
fn invoice_item_point_in_time_recognition_lowers_to_domain() {
    let body = serde_json::json!({
        "amount_minor_ex_tax": 500,
        "currency": "USD",
        "revenue_stream": "usage",
        "catalog_class": "REVENUE",
        "recognition": { "policy_ref": "policy.pit.v1", "timing": "point_in_time" }
    });
    let dto: InvoiceItemDto =
        serde_json::from_value(body).expect("snake_case item must deserialize");
    let item = dto.into_domain().expect("valid recognition block");
    let rec = item.recognition.expect("recognition present");
    assert!(matches!(rec.timing, RecognitionTiming::PointInTime));
}

/// A `straight_line` block WITHOUT `periods` is rejected at the boundary
/// (`InvalidRequest` ⇒ HTTP 400), never a panic or a silent default.
#[test]
fn invoice_item_straight_line_without_periods_is_invalid_request() {
    let body = serde_json::json!({
        "amount_minor_ex_tax": 1200,
        "currency": "USD",
        "revenue_stream": "subscription",
        "catalog_class": "REVENUE",
        "recognition": { "policy_ref": "p", "timing": "straight_line" }
    });
    let dto: InvoiceItemDto =
        serde_json::from_value(body).expect("snake_case item must deserialize");
    let err = dto
        .into_domain()
        .expect_err("straight_line without periods must reject");
    assert!(matches!(err, DomainError::InvalidRequest(_)), "got {err:?}");
}

/// An unknown `timing` literal is rejected at the boundary (`InvalidRequest`).
#[test]
fn invoice_item_unknown_timing_is_invalid_request() {
    let body = serde_json::json!({
        "amount_minor_ex_tax": 100,
        "currency": "USD",
        "revenue_stream": "subscription",
        "catalog_class": "REVENUE",
        "recognition": { "policy_ref": "p", "timing": "milestone" }
    });
    let dto: InvoiceItemDto =
        serde_json::from_value(body).expect("snake_case item must deserialize");
    let err = dto
        .into_domain()
        .expect_err("an unknown timing must reject");
    assert!(matches!(err, DomainError::InvalidRequest(_)), "got {err:?}");
}

/// An invoice whose items/tax carry DIFFERENT currencies is rejected at the
/// boundary (`InvalidRequest`) — the builder would otherwise stamp the first
/// currency on every line and silently misattribute the others.
#[test]
fn mixed_currency_invoice_is_invalid_request() {
    let tenant = uuid::uuid!("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
    let actor = uuid::uuid!("99999999-8888-7777-6666-555555555555");
    let body = serde_json::json!({
        "tenant_id": tenant,
        "invoice_id": "INV-MIX",
        "payer_tenant_id": tenant,
        "effective_at": "2026-06-01",
        "period_id": "202606",
        "items": [
            {"amount_minor_ex_tax": 1000, "currency": "USD", "revenue_stream": "subscription"},
            {"amount_minor_ex_tax": 500, "currency": "EUR", "revenue_stream": "usage"}
        ],
        "tax": [],
        "correlation_id": actor
    });
    let dto: PostInvoiceRequestDto =
        serde_json::from_value(body).expect("snake_case body must deserialize");
    let err = dto
        .into_domain(actor)
        .expect_err("a mixed-currency invoice must reject");
    assert!(
        matches!(err, DomainError::InvalidRequest(_)),
        "expected InvalidRequest, got {err:?}"
    );
}

/// A malformed currency code is rejected at the boundary as `InvalidRequest` (a
/// 400) rather than reaching a persisted line where it would match zero
/// currency-scale rows.
#[test]
fn invoice_item_invalid_currency_is_invalid_request() {
    let dto = InvoiceItemDto {
        amount_minor_ex_tax: 100,
        currency: "NOT-A-CURRENCY".to_owned(), // > 10 chars ⇒ over the code cap
        revenue_stream: "subscription".to_owned(),
        catalog_class: None,
        contract_class: None,
        gl_code: None,
        recognition: None,
        invoice_item_ref: None,
        sku_or_plan_ref: None,
        price_id: None,
        pricing_snapshot_ref: None,
    };
    let err = dto
        .into_domain()
        .expect_err("a malformed currency must reject");
    assert!(
        matches!(err, DomainError::InvalidRequest(_)),
        "expected InvalidRequest, got {err:?}"
    );
}

/// A wholly-empty invoice (no items AND no tax) is rejected at the boundary as
/// `InvalidRequest` — a zero-line invoice has nothing to post.
#[test]
fn empty_invoice_is_invalid_request() {
    let tenant = uuid::uuid!("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
    let actor = uuid::uuid!("99999999-8888-7777-6666-555555555555");
    let body = serde_json::json!({
        "tenant_id": tenant,
        "invoice_id": "INV-EMPTY",
        "payer_tenant_id": tenant,
        "effective_at": "2026-06-01",
        "period_id": "202606",
        "items": [],
        "tax": [],
        "correlation_id": actor
    });
    let dto: PostInvoiceRequestDto =
        serde_json::from_value(body).expect("snake_case body must deserialize");
    let err = dto
        .into_domain(actor)
        .expect_err("a zero-line invoice must reject");
    assert!(
        matches!(err, DomainError::InvalidRequest(_)),
        "expected InvalidRequest, got {err:?}"
    );
}

/// An over-long persisted free-text field is capped at the boundary as
/// `InvalidRequest` (a 400) rather than blowing past its storage column as a 500.
#[test]
fn over_long_free_text_is_rejected_at_the_boundary() {
    let dto = MappingCorrectionRequestDto {
        reason: "X".repeat(MAX_FREE_TEXT_LEN + 1), // one over the free-text cap
        period_id: None,
        effective_at: None,
        corrected_items: vec![],
    };
    let err = dto
        .corrected_items_into_domain()
        .expect_err("an over-long reason must reject");
    assert!(
        matches!(err, DomainError::InvalidRequest(_)),
        "expected InvalidRequest, got {err:?}"
    );
}

/// The handler-lowered DTOs (no `into_domain`) cap their free text via a
/// `validate()` the handler calls: an over-long reversal `reason` and an
/// over-long re-identify `reason_code` both reject as `InvalidRequest`.
#[test]
fn handler_lowered_dtos_cap_free_text_via_validate() {
    let reversal = ReversalRequestDto {
        reason: "X".repeat(MAX_FREE_TEXT_LEN + 1),
        period_id: None,
        effective_at: None,
    };
    assert!(
        matches!(reversal.validate(), Err(DomainError::InvalidRequest(_))),
        "an over-long reversal reason must reject"
    );

    let reidentify = ReidentifyRequestDto {
        payer_tenant_id: uuid::Uuid::now_v7(),
        reason_code: "X".repeat(MAX_REASON_CODE_LEN + 1),
        target_scope: None,
    };
    assert!(
        matches!(reidentify.validate(), Err(DomainError::InvalidRequest(_))),
        "an over-long reason_code must reject"
    );
}

/// An allocate body WITHOUT `splits` lowers to the SDK type with `splits: None`
/// (Mode A/B precedence path — unchanged), and `payment_id` is bound from the
/// PATH (not the body).
#[test]
fn allocate_body_without_splits_lowers_with_none() {
    let tenant = uuid::uuid!("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
    let payer = uuid::uuid!("11111111-2222-3333-4444-555555555555");
    let body = serde_json::json!({
        "tenant_id": tenant,
        "payer_tenant_id": payer,
        "allocation_id": uuid::Uuid::now_v7(),
        "lump_minor": 500,
        "currency": "USD",
        "scale": 2
    });
    let dto: AllocatePaymentRequest =
        serde_json::from_value(body).expect("snake_case body must deserialize");
    let sdk = dto
        .into_sdk("PAY-1".to_owned())
        .expect("valid ids lower cleanly");
    assert_eq!(sdk.payment_id, "PAY-1", "payment_id bound from the path");
    assert!(sdk.splits.is_none(), "no caller split ⇒ precedence path");
}

/// An allocate body WITH `splits` (Mode B) deserializes `snake_case` and lowers
/// each share into the SDK `AllocationSplit`, preserving order.
#[test]
fn allocate_body_with_splits_lowers_to_sdk_split() {
    let tenant = uuid::uuid!("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
    let payer = uuid::uuid!("11111111-2222-3333-4444-555555555555");
    let body = serde_json::json!({
        "tenant_id": tenant,
        "payer_tenant_id": payer,
        "allocation_id": uuid::Uuid::now_v7(),
        "lump_minor": 500,
        "currency": "USD",
        "scale": 2,
        "splits": [
            { "invoice_id": "INV-B", "amount_minor": 300 },
            { "invoice_id": "INV-A", "amount_minor": 200 }
        ]
    });
    let dto: AllocatePaymentRequest =
        serde_json::from_value(body).expect("snake_case body must deserialize");
    let sdk = dto
        .into_sdk("PAY-1".to_owned())
        .expect("valid ids lower cleanly");
    let splits = sdk.splits.expect("splits present");
    assert_eq!(splits.len(), 2);
    assert_eq!(splits[0].invoice_id, "INV-B");
    assert_eq!(splits[0].amount_minor, 300);
    assert_eq!(splits[1].invoice_id, "INV-A");
    assert_eq!(splits[1].amount_minor, 200);
}

/// An over-long business id is rejected at the DTO boundary as a clean
/// `InvalidRequest` (⇒ HTTP 400) rather than reaching a `varchar(128)` column as
/// a 500. Covers a PATH-bound id (`payment_id`) and a BODY id (`psp_return_id`) —
/// the shared `validate_business_id` guards every id the same way.
#[test]
fn over_long_business_id_is_rejected_at_the_boundary() {
    let tenant = uuid::uuid!("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
    let payer = uuid::uuid!("11111111-2222-3333-4444-555555555555");
    let too_long = "X".repeat(MAX_BUSINESS_ID_LEN + 1); // one over the varchar(128) bound

    // PATH-bound payment_id on a return: rejected.
    let ret: ReturnPaymentRequest = serde_json::from_value(serde_json::json!({
        "tenant_id": tenant,
        "payer_tenant_id": payer,
        "psp_return_id": "RET-1",
        "amount_minor": 100,
        "currency": "USD",
        "scale": 2
    }))
    .expect("body deserializes");
    assert!(
        matches!(
            ret.into_sdk(too_long.clone()),
            Err(DomainError::InvalidRequest(_))
        ),
        "an over-long payment_id must reject at the boundary, not reach the column"
    );

    // BODY-supplied psp_return_id: also rejected (valid path id, bad body id).
    let ret2: ReturnPaymentRequest = serde_json::from_value(serde_json::json!({
        "tenant_id": tenant,
        "payer_tenant_id": payer,
        "psp_return_id": too_long,
        "amount_minor": 100,
        "currency": "USD",
        "scale": 2
    }))
    .expect("body deserializes");
    assert!(
        matches!(
            ret2.into_sdk("PAY-1".to_owned()),
            Err(DomainError::InvalidRequest(_))
        ),
        "an over-long psp_return_id must reject at the boundary"
    );

    // An empty id is likewise rejected (not silently accepted as a blank key).
    let ret3: ReturnPaymentRequest = serde_json::from_value(serde_json::json!({
        "tenant_id": tenant,
        "payer_tenant_id": payer,
        "psp_return_id": "",
        "amount_minor": 100,
        "currency": "USD",
        "scale": 2
    }))
    .expect("body deserializes");
    assert!(
        matches!(
            ret3.into_sdk("PAY-1".to_owned()),
            Err(DomainError::InvalidRequest(_))
        ),
        "an empty psp_return_id must reject at the boundary"
    );
}

/// A `kind = "grant"` credit-application body deserializes `snake_case` and
/// `into_sdk` lowers it to the SDK `CreditApplication::Grant` (the grant fields
/// are required; `targets` is moot).
#[test]
fn credit_application_grant_body_lowers_to_sdk_grant() {
    let tenant = uuid::uuid!("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
    let payer = uuid::uuid!("11111111-2222-3333-4444-555555555555");
    let body = serde_json::json!({
        "kind": "grant",
        "tenant_id": tenant,
        "payer_tenant_id": payer,
        "credit_application_id": "CA-1",
        "currency": "USD",
        "scale": 2,
        "amount_minor": 1500,
        "credit_grant_event_type": "PROMO"
    });
    let dto: CreditApplicationRequest =
        serde_json::from_value(body).expect("snake_case body must deserialize");
    let sdk = dto.into_sdk().expect("grant lowers cleanly");
    let bss_ledger_sdk::CreditApplication::Grant(g) = sdk else {
        panic!("expected a Grant");
    };
    assert_eq!(g.tenant_id, tenant, "tenant = body tenant_id");
    assert_eq!(g.payer_tenant_id, payer);
    assert_eq!(g.credit_application_id, "CA-1");
    assert_eq!(g.amount_minor, 1500);
    assert_eq!(g.credit_grant_event_type, "PROMO");
}

/// A `kind = "apply"` credit-application body deserializes `snake_case` and
/// `into_sdk` lowers each target into the SDK `AllocationSplit`, preserving order
/// (the grant fields are moot).
#[test]
fn credit_application_apply_body_lowers_to_sdk_apply() {
    let tenant = uuid::uuid!("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
    let payer = uuid::uuid!("11111111-2222-3333-4444-555555555555");
    let body = serde_json::json!({
        "kind": "apply",
        "tenant_id": tenant,
        "payer_tenant_id": payer,
        "credit_application_id": "CA-2",
        "currency": "USD",
        "scale": 2,
        "targets": [
            { "invoice_id": "INV-B", "amount_minor": 300 },
            { "invoice_id": "INV-A", "amount_minor": 200 }
        ]
    });
    let dto: CreditApplicationRequest =
        serde_json::from_value(body).expect("snake_case body must deserialize");
    let sdk = dto.into_sdk().expect("apply lowers cleanly");
    let bss_ledger_sdk::CreditApplication::Apply(a) = sdk else {
        panic!("expected an Apply");
    };
    assert_eq!(a.credit_application_id, "CA-2");
    assert_eq!(a.targets.len(), 2);
    assert_eq!(a.targets[0].invoice_id, "INV-B");
    assert_eq!(a.targets[0].amount_minor, 300);
    assert_eq!(a.targets[1].invoice_id, "INV-A");
    assert_eq!(a.targets[1].amount_minor, 200);
}

/// A `grant` missing `amount_minor` is rejected `400 InvalidArgument` in
/// `into_sdk` (the kind-specific shape is validated at the boundary, not deep in
/// the post path).
#[test]
fn credit_application_grant_missing_amount_is_invalid() {
    let tenant = uuid::uuid!("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
    let payer = uuid::uuid!("11111111-2222-3333-4444-555555555555");
    let body = serde_json::json!({
        "kind": "grant",
        "tenant_id": tenant,
        "payer_tenant_id": payer,
        "credit_application_id": "CA-3",
        "currency": "USD",
        "scale": 2,
        "credit_grant_event_type": "PROMO"
    });
    let dto: CreditApplicationRequest =
        serde_json::from_value(body).expect("snake_case body must deserialize");
    let err = dto
        .into_sdk()
        .expect_err("a grant without amount_minor must reject");
    assert_eq!(
        err.status_code(),
        400,
        "expected a 400 InvalidArgument, got {err:?}"
    );
}

/// An apply with an empty `targets` is rejected `400 InvalidArgument` (an empty
/// split set has nothing to spend the wallet on).
#[test]
fn credit_application_apply_empty_targets_is_invalid() {
    let tenant = uuid::uuid!("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
    let payer = uuid::uuid!("11111111-2222-3333-4444-555555555555");
    let body = serde_json::json!({
        "kind": "apply",
        "tenant_id": tenant,
        "payer_tenant_id": payer,
        "credit_application_id": "CA-4",
        "currency": "USD",
        "scale": 2,
        "targets": []
    });
    let dto: CreditApplicationRequest =
        serde_json::from_value(body).expect("snake_case body must deserialize");
    assert!(
        dto.into_sdk().is_err(),
        "an apply with empty targets must reject"
    );
}

/// An unknown `kind` is rejected `400 InvalidArgument` (neither grant nor apply).
#[test]
fn credit_application_unknown_kind_is_invalid() {
    let tenant = uuid::uuid!("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
    let payer = uuid::uuid!("11111111-2222-3333-4444-555555555555");
    let body = serde_json::json!({
        "kind": "refund",
        "tenant_id": tenant,
        "payer_tenant_id": payer,
        "credit_application_id": "CA-5",
        "currency": "USD",
        "scale": 2
    });
    let dto: CreditApplicationRequest =
        serde_json::from_value(body).expect("snake_case body must deserialize");
    assert!(dto.into_sdk().is_err(), "an unknown kind must reject");
}

/// A `replace` change body deserializes (`snake_case`) and lowers to the SDK
/// command, binding `schedule_id` from the PATH and mapping `new_segments`.
#[test]
fn change_schedule_replace_body_lowers_to_sdk() {
    let tenant = uuid::uuid!("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
    let body = serde_json::json!({
        "tenant_id": tenant,
        "change_id": "CHG-1",
        "action": "replace",
        "treatment": "prospective",
        "new_segments": [
            { "period_id": "202607", "amount_minor": 300 },
            { "period_id": "202608", "amount_minor": 500 }
        ]
    });
    let dto: ChangeRecognitionScheduleRequest =
        serde_json::from_value(body).expect("snake_case body must deserialize");
    let cmd = dto.into_sdk("SCH-1".to_owned()).expect("lowers to sdk");
    assert_eq!(cmd.tenant_id, tenant);
    assert_eq!(
        cmd.schedule_id, "SCH-1",
        "schedule_id is bound from the PATH"
    );
    assert_eq!(cmd.change_id, "CHG-1");
    assert_eq!(cmd.action, "replace");
    assert_eq!(cmd.treatment, "prospective");
    let segs = cmd.new_segments.expect("replace carries segments");
    assert_eq!(segs.len(), 2);
    assert_eq!(segs[0].period_id, "202607");
    assert_eq!(segs[0].amount_minor, 300);
    assert_eq!(segs[1].amount_minor, 500);
}

/// A `cancel` change body lowers with `new_segments = None`.
#[test]
fn change_schedule_cancel_body_lowers_with_no_segments() {
    let tenant = uuid::uuid!("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
    let body = serde_json::json!({
        "tenant_id": tenant,
        "change_id": "CHG-2",
        "action": "cancel",
        "treatment": "prospective"
    });
    let dto: ChangeRecognitionScheduleRequest =
        serde_json::from_value(body).expect("snake_case body must deserialize");
    let cmd = dto.into_sdk("SCH-2".to_owned()).expect("lowers to sdk");
    assert_eq!(cmd.action, "cancel");
    assert!(cmd.new_segments.is_none(), "cancel carries no segments");
}

/// An empty `change_id` is rejected at the boundary (the `varchar(128)` business
/// id convention), not deep in the change service.
#[test]
fn change_schedule_empty_change_id_is_rejected() {
    let tenant = uuid::uuid!("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
    let body = serde_json::json!({
        "tenant_id": tenant,
        "change_id": "",
        "action": "cancel",
        "treatment": "prospective"
    });
    let dto: ChangeRecognitionScheduleRequest =
        serde_json::from_value(body).expect("snake_case body must deserialize");
    assert!(
        dto.into_sdk("SCH-3".to_owned()).is_err(),
        "an empty change_id must reject"
    );
}

/// The invoice-post response serializes the materialized schedules as a
/// `snake_case` `schedules` array (the discovery contract that lets a REST client
/// learn the server-minted `schedule_id`). Pins the wire field names so a rename
/// can't silently break a client.
#[test]
fn post_invoice_response_serializes_materialized_schedules() {
    let dto = PostInvoiceResponseDto {
        entry_id: uuid::uuid!("11111111-1111-1111-1111-111111111111"),
        created_seq: 7,
        replayed: false,
        schedules: vec![MaterializedScheduleDto {
            schedule_id: "SCH-1".to_owned(),
            revenue_stream: "subscription".to_owned(),
            source_invoice_item_ref: "ITEM-1".to_owned(),
        }],
    };
    let v = serde_json::to_value(&dto).expect("serialize");
    assert_eq!(v["created_seq"], serde_json::json!(7));
    assert_eq!(v["replayed"], serde_json::json!(false));
    assert_eq!(v["schedules"][0]["schedule_id"], serde_json::json!("SCH-1"));
    assert_eq!(
        v["schedules"][0]["revenue_stream"],
        serde_json::json!("subscription")
    );
    assert_eq!(
        v["schedules"][0]["source_invoice_item_ref"],
        serde_json::json!("ITEM-1")
    );
}

/// The `GET /recognition-schedules` list response is a `snake_case` `schedules`
/// array of HEADER views — no `segments` key (those live on the by-id read), an
/// absent optional ref serializes as an explicit `null` (not a dropped key), and
/// the envelope carries the `truncated` cap signal. Pins EVERY header field's
/// wire name so a rename can't silently break a client.
#[test]
fn recognition_schedule_list_response_is_header_only() {
    let dto = RecognitionScheduleListResponse {
        schedules: vec![RecognitionScheduleSummaryDto {
            schedule_id: "SCH-9".to_owned(),
            status: "ACTIVE".to_owned(),
            version: 0,
            revenue_stream: "support".to_owned(),
            currency: "USD".to_owned(),
            total_deferred_minor: 1200,
            recognized_minor: 100,
            source_invoice_id: "INV-9".to_owned(),
            source_invoice_item_ref: "ITEM-9".to_owned(),
            po_allocation_group: None,
            subscription_ref: None,
            policy_ref: "pol-1".to_owned(),
        }],
        truncated: false,
    };
    let v = serde_json::to_value(&dto).expect("serialize");
    assert_eq!(
        v["truncated"],
        serde_json::json!(false),
        "cap signal present"
    );
    let row = &v["schedules"][0];
    // Pin EVERY header field's wire name (rename guard).
    assert_eq!(row["schedule_id"], serde_json::json!("SCH-9"));
    assert_eq!(row["status"], serde_json::json!("ACTIVE"));
    assert_eq!(row["version"], serde_json::json!(0));
    assert_eq!(row["revenue_stream"], serde_json::json!("support"));
    assert_eq!(row["currency"], serde_json::json!("USD"));
    assert_eq!(row["total_deferred_minor"], serde_json::json!(1200));
    assert_eq!(row["recognized_minor"], serde_json::json!(100));
    assert_eq!(row["source_invoice_id"], serde_json::json!("INV-9"));
    assert_eq!(row["source_invoice_item_ref"], serde_json::json!("ITEM-9"));
    assert_eq!(row["policy_ref"], serde_json::json!("pol-1"));
    assert!(
        row.get("segments").is_none(),
        "the list view is header-only (segments live on the by-id read)"
    );
    assert!(
        matches!(
            row.get("po_allocation_group"),
            Some(serde_json::Value::Null)
        ),
        "an absent optional ref is an explicit null, not a dropped key"
    );
    assert!(
        matches!(row.get("subscription_ref"), Some(serde_json::Value::Null)),
        "subscription_ref absent ⇒ explicit null"
    );
}

// ── Read-surface view `From<Model>` mappings (refund / notes / dispute /
// recognition-run / settlement / entry-header / payer-state) ─────────────────────
//
// Each test builds the full entity `Model` with a DISTINCT sentinel per field,
// converts via `View::from(model)`, and asserts every view field equals the
// source. The EXCLUDED columns (`tenant_id`, the optimistic-`version` counter,
// the entry hash-chain internals, …) are simply not present on the view — the
// struct literal compiles without reading them, which is the implicit guard that
// they stay off the wire.

/// `RefundView::from(refund::Model)` maps every surfaced field; the entity's
/// `tenant_id` / `created_at_utc` / `version` are intentionally NOT on the view.
#[test]
fn refund_view_from_model_maps_all_fields() {
    let model = crate::infra::storage::entity::refund::Model {
        tenant_id: uuid::uuid!("11111111-1111-1111-1111-111111111111"),
        refund_id: "RFND-1".to_owned(),
        psp_refund_id: "PSP-RFND-1".to_owned(),
        phase: "CLEARED".to_owned(),
        pattern: "A_UNALLOCATED".to_owned(),
        payment_id: "PAY-1".to_owned(),
        invoice_id: Some("INV-1".to_owned()),
        currency: "USD".to_owned(),
        amount_minor: 1234,
        clearing_state: "SETTLED".to_owned(),
        relates_to_refund_id: Some("RFND-0".to_owned()),
        reverses_entry_id: Some(uuid::uuid!("22222222-2222-2222-2222-222222222222")),
        created_at_utc: from_unix(1_700_000_001, 0).expect("ts"),
        version: 7,
    };
    let view = RefundView::from(model);
    assert_eq!(view.refund_id, "RFND-1");
    assert_eq!(view.psp_refund_id, "PSP-RFND-1");
    assert_eq!(view.phase, "CLEARED");
    assert_eq!(view.pattern, "A_UNALLOCATED");
    assert_eq!(view.payment_id, "PAY-1");
    assert_eq!(view.invoice_id.as_deref(), Some("INV-1"));
    assert_eq!(view.currency, "USD");
    assert_eq!(view.amount_minor, 1234);
    assert_eq!(view.clearing_state, "SETTLED");
    assert_eq!(view.relates_to_refund_id.as_deref(), Some("RFND-0"));
    assert_eq!(
        view.reverses_entry_id,
        Some(uuid::uuid!("22222222-2222-2222-2222-222222222222"))
    );
}

/// `CreditNoteView::from(credit_note::Model)` maps every surfaced field
/// (including `created_at_utc`); the entity's `tenant_id` is NOT on the view.
#[test]
fn credit_note_view_from_model_maps_all_fields() {
    let created = from_unix(1_700_000_002, 0).expect("ts");
    let model = crate::infra::storage::entity::credit_note::Model {
        tenant_id: uuid::uuid!("11111111-1111-1111-1111-111111111111"),
        credit_note_id: "CN-1".to_owned(),
        origin_invoice_id: "INV-1".to_owned(),
        origin_invoice_item_ref: Some("ITEM-1".to_owned()),
        revenue_stream: "subscription".to_owned(),
        currency: "USD".to_owned(),
        amount_minor: 5000,
        recognized_part_minor: 3000,
        deferred_part_minor: 1500,
        split_basis_ref: Some("SCH-1".to_owned()),
        reason_code: "GOODWILL".to_owned(),
        created_at_utc: created,
    };
    let view = CreditNoteView::from(model);
    assert_eq!(view.credit_note_id, "CN-1");
    assert_eq!(view.origin_invoice_id, "INV-1");
    assert_eq!(view.origin_invoice_item_ref.as_deref(), Some("ITEM-1"));
    assert_eq!(view.revenue_stream, "subscription");
    assert_eq!(view.currency, "USD");
    assert_eq!(view.amount_minor, 5000);
    assert_eq!(view.recognized_part_minor, 3000);
    assert_eq!(view.deferred_part_minor, 1500);
    assert_eq!(view.split_basis_ref.as_deref(), Some("SCH-1"));
    assert_eq!(view.reason_code, "GOODWILL");
    assert_eq!(view.created_at_utc, created);
}

/// `DebitNoteView::from(debit_note::Model)` maps every surfaced field; the entity's
/// `tenant_id` is NOT on the view. The debit note is leaner than the credit note
/// (no `revenue_stream` / `reason_code` / item ref).
#[test]
fn debit_note_view_from_model_maps_all_fields() {
    let created = from_unix(1_700_000_003, 0).expect("ts");
    let model = crate::infra::storage::entity::debit_note::Model {
        tenant_id: uuid::uuid!("11111111-1111-1111-1111-111111111111"),
        debit_note_id: "DN-1".to_owned(),
        origin_invoice_id: "INV-2".to_owned(),
        currency: "EUR".to_owned(),
        amount_minor: 9000,
        recognized_part_minor: 6000,
        deferred_part_minor: 2500,
        created_at_utc: created,
    };
    let view = DebitNoteView::from(model);
    assert_eq!(view.debit_note_id, "DN-1");
    assert_eq!(view.origin_invoice_id, "INV-2");
    assert_eq!(view.currency, "EUR");
    assert_eq!(view.amount_minor, 9000);
    assert_eq!(view.recognized_part_minor, 6000);
    assert_eq!(view.deferred_part_minor, 2500);
    assert_eq!(view.created_at_utc, created);
}

/// `DisputeView::from(dispute::Model)` maps every surfaced field; the entity's
/// `tenant_id` / `version` (the optimistic-concurrency counter) are NOT on the
/// view.
#[test]
fn dispute_view_from_model_maps_all_fields() {
    let model = crate::infra::storage::entity::dispute::Model {
        tenant_id: uuid::uuid!("11111111-1111-1111-1111-111111111111"),
        dispute_id: "DSP-1".to_owned(),
        payment_id: "PAY-1".to_owned(),
        currency: "USD".to_owned(),
        variant: "CASH_HOLD".to_owned(),
        last_phase: "OPENED".to_owned(),
        cycle: 2,
        disputed_amount_minor: 4200,
        cash_hold_minor: 4000,
        version: 9,
    };
    let view = DisputeView::from(model);
    assert_eq!(view.dispute_id, "DSP-1");
    assert_eq!(view.payment_id, "PAY-1");
    assert_eq!(view.currency, "USD");
    assert_eq!(view.variant, "CASH_HOLD");
    assert_eq!(view.last_phase, "OPENED");
    assert_eq!(view.cycle, 2);
    assert_eq!(view.disputed_amount_minor, 4200);
    assert_eq!(view.cash_hold_minor, 4000);
}

/// `RecognitionRunView::from(recognition_run::Model)` maps every surfaced field;
/// the entity's `tenant_id` is NOT on the view.
#[test]
fn recognition_run_view_from_model_maps_all_fields() {
    let started = from_unix(1_700_000_004, 0).expect("ts");
    let run_id = uuid::uuid!("33333333-3333-3333-3333-333333333333");
    let model = crate::infra::storage::entity::recognition_run::Model {
        tenant_id: uuid::uuid!("11111111-1111-1111-1111-111111111111"),
        period_id: "202606".to_owned(),
        run_id,
        started_at_utc: started,
        status: "DONE".to_owned(),
    };
    let view = RecognitionRunView::from(model);
    assert_eq!(view.run_id, run_id);
    assert_eq!(view.period_id, "202606");
    assert_eq!(view.status, "DONE");
    assert_eq!(view.started_at_utc, started);
}

/// `SettlementView::from(payment_settlement::Model)` maps every surfaced counter;
/// the entity's `tenant_id` / `version` (the optimistic-concurrency counter) are
/// NOT on the view.
#[test]
fn settlement_view_from_model_maps_all_fields() {
    let model = crate::infra::storage::entity::payment_settlement::Model {
        tenant_id: uuid::uuid!("11111111-1111-1111-1111-111111111111"),
        payment_id: "PAY-1".to_owned(),
        currency: "USD".to_owned(),
        settled_minor: 10_000,
        fee_minor: 300,
        allocated_minor: 6000,
        refunded_minor: 1500,
        refunded_unallocated_minor: 500,
        clawed_back_minor: 200,
        version: 11,
    };
    let view = SettlementView::from(model);
    assert_eq!(view.payment_id, "PAY-1");
    assert_eq!(view.currency, "USD");
    assert_eq!(view.settled_minor, 10_000);
    assert_eq!(view.fee_minor, 300);
    assert_eq!(view.allocated_minor, 6000);
    assert_eq!(view.refunded_minor, 1500);
    assert_eq!(view.refunded_unallocated_minor, 500);
    assert_eq!(view.clawed_back_minor, 200);
}

/// `EntryHeaderView::from(journal_entry::Model)` maps the surfaced header dims;
/// the entity's `tenant_id`, `legal_entity_id`, `reverses_period_id`,
/// `posted_by_actor_id`, `correlation_id`, `rounding_evidence`, and the
/// tamper-evidence hash-chain internals (`row_hash` / `prev_hash` /
/// `prev_entry_id` / `prev_period_id`) are NOT on the lightweight header view.
#[test]
fn entry_header_view_from_model_maps_all_fields() {
    let posted = from_unix(1_700_000_005, 0).expect("ts");
    let effective = chrono::NaiveDate::from_ymd_opt(2026, 6, 27).expect("date");
    let entry_id = uuid::uuid!("44444444-4444-4444-4444-444444444444");
    let reverses = uuid::uuid!("55555555-5555-5555-5555-555555555555");
    let model = crate::infra::storage::entity::journal_entry::Model {
        entry_id,
        tenant_id: uuid::uuid!("11111111-1111-1111-1111-111111111111"),
        legal_entity_id: uuid::uuid!("66666666-6666-6666-6666-666666666666"),
        period_id: "202606".to_owned(),
        entry_currency: "USD".to_owned(),
        source_doc_type: "REFUND".to_owned(),
        source_business_id: "RFND-1".to_owned(),
        reverses_entry_id: Some(reverses),
        reverses_period_id: Some("202605".to_owned()),
        posted_at_utc: posted,
        effective_at: effective,
        origin: "PSP".to_owned(),
        posted_by_actor_id: uuid::uuid!("77777777-7777-7777-7777-777777777777"),
        correlation_id: uuid::uuid!("88888888-8888-8888-8888-888888888888"),
        rounding_evidence: serde_json::json!({ "mode": "BANKERS" }),
        created_seq: 42,
        row_hash: Some(vec![1, 2, 3]),
        prev_hash: Some(vec![4, 5, 6]),
        prev_entry_id: Some(uuid::uuid!("99999999-9999-9999-9999-999999999999")),
        prev_period_id: Some("202604".to_owned()),
    };
    let view = EntryHeaderView::from(model);
    assert_eq!(view.entry_id, entry_id);
    assert_eq!(view.period_id, "202606");
    assert_eq!(view.entry_currency, "USD");
    assert_eq!(view.source_doc_type, "REFUND");
    assert_eq!(view.source_business_id, "RFND-1");
    assert_eq!(view.reverses_entry_id, Some(reverses));
    assert_eq!(view.posted_at_utc, posted);
    assert_eq!(view.effective_at, effective);
    assert_eq!(view.origin, "PSP");
    assert_eq!(view.created_seq, 42);
}

/// `PayerStateView::from(payer_state::Model)` maps every surfaced field; the
/// entity's `tenant_id` is NOT on the view.
#[test]
fn payer_state_view_from_model_maps_all_fields() {
    let changed = from_unix(1_700_000_006, 0).expect("ts");
    let approver = uuid::uuid!("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
    let payer = uuid::uuid!("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb");
    let model = crate::infra::storage::entity::payer_state::Model {
        tenant_id: uuid::uuid!("11111111-1111-1111-1111-111111111111"),
        payer_tenant_id: payer,
        lifecycle_state: "CLOSED".to_owned(),
        closed_with_open_balance: true,
        approved_by: Some(approver),
        changed_at: Some(changed),
    };
    let view = PayerStateView::from(model);
    assert_eq!(view.payer_tenant_id, payer);
    assert_eq!(view.lifecycle_state, "CLOSED");
    assert!(view.closed_with_open_balance);
    assert_eq!(view.approved_by, Some(approver));
    assert_eq!(view.changed_at, Some(changed));
}

/// `DualControlPolicyView::from_effective(Some)` surfaces the configured version's
/// thresholds + `version`/`effective_from` provenance and is NOT flagged default.
#[test]
fn dual_control_policy_view_from_configured_version() {
    let eff = from_unix(1_700_000_000, 0).expect("ts");
    let view = DualControlPolicyView::from_effective(Some(PolicyVersion {
        effective_from: eff,
        version: 3,
        policy: DualControlPolicy {
            d2_threshold_minor: 250_000,
            a6_backdating_biz_days: 7,
            pending_ttl_seconds: 3_600,
        },
    }));
    assert_eq!(view.d2_threshold_minor, 250_000);
    assert_eq!(view.a6_backdating_biz_days, 7);
    assert_eq!(view.pending_ttl_seconds, 3_600);
    assert_eq!(view.effective_from, Some(eff));
    assert_eq!(view.version, Some(3));
    assert!(!view.is_default);
}

/// `DualControlPolicyView::from_effective(None)` renders the ratified platform
/// defaults and flags `is_default` (the tenant has no policy row).
#[test]
fn dual_control_policy_view_from_none_yields_platform_defaults() {
    let view = DualControlPolicyView::from_effective(None);
    let d = DualControlPolicy::DEFAULT;
    assert_eq!(view.d2_threshold_minor, d.d2_threshold_minor);
    assert_eq!(view.a6_backdating_biz_days, d.a6_backdating_biz_days);
    assert_eq!(view.pending_ttl_seconds, d.pending_ttl_seconds);
    assert_eq!(view.effective_from, None);
    assert_eq!(view.version, None);
    assert!(view.is_default);
}

// ── FX rate ingest validation (Slice 5) ──────────────────────────────────────

/// A `snake_case` `POST /fx/rates` body (`fallback_order` omitted).
fn fx_ingest_body(base: &str, quote: &str, provider: &str, rate_micro: i64) -> serde_json::Value {
    serde_json::json!({
        "tenant_id": "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
        "base_currency": base,
        "quote_currency": quote,
        "provider": provider,
        "rate_micro": rate_micro,
        "as_of": "2026-06-27T00:00:00Z",
    })
}

#[test]
fn fx_ingest_valid_body_validates_and_defaults_fallback_order() {
    let req: FxRateIngestRequest =
        serde_json::from_value(fx_ingest_body("EUR", "USD", "ecb", 1_100_000)).unwrap();
    assert_eq!(req.validate().unwrap(), 0, "fallback_order defaults to 0");
}

#[test]
fn fx_ingest_rejects_identity_pair() {
    // base == quote is a no-op rate the lock-time short-circuit never reads.
    let req: FxRateIngestRequest =
        serde_json::from_value(fx_ingest_body("USD", "USD", "ecb", 1_000_000)).unwrap();
    assert!(matches!(
        req.validate(),
        Err(DomainError::InvalidRequest(_))
    ));
}

#[test]
fn fx_ingest_rejects_non_positive_rate() {
    let zero: FxRateIngestRequest =
        serde_json::from_value(fx_ingest_body("EUR", "USD", "ecb", 0)).unwrap();
    assert!(matches!(
        zero.validate(),
        Err(DomainError::InvalidRequest(_))
    ));
    let negative: FxRateIngestRequest =
        serde_json::from_value(fx_ingest_body("EUR", "USD", "ecb", -5)).unwrap();
    assert!(matches!(
        negative.validate(),
        Err(DomainError::InvalidRequest(_))
    ));
}

#[test]
fn fx_ingest_rejects_empty_currency_and_provider() {
    let bad_ccy: FxRateIngestRequest =
        serde_json::from_value(fx_ingest_body("", "USD", "ecb", 1_000_000)).unwrap();
    assert!(matches!(
        bad_ccy.validate(),
        Err(DomainError::InvalidRequest(_))
    ));
    let bad_provider: FxRateIngestRequest =
        serde_json::from_value(fx_ingest_body("EUR", "USD", "", 1_000_000)).unwrap();
    assert!(matches!(
        bad_provider.validate(),
        Err(DomainError::InvalidRequest(_))
    ));
}

#[test]
fn fx_ingest_passes_explicit_fallback_order_through() {
    let mut body = fx_ingest_body("EUR", "USD", "ecb", 1_100_000);
    body["fallback_order"] = serde_json::json!(2);
    let req: FxRateIngestRequest = serde_json::from_value(body).unwrap();
    assert_eq!(req.validate().unwrap(), 2);
}

#[test]
fn fx_ingest_rejects_negative_fallback_order() {
    let mut body = fx_ingest_body("EUR", "USD", "ecb", 1_100_000);
    body["fallback_order"] = serde_json::json!(-1);
    let req: FxRateIngestRequest = serde_json::from_value(body).unwrap();
    assert!(matches!(
        req.validate(),
        Err(DomainError::InvalidRequest(_))
    ));
}
