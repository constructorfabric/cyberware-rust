//! Pure mapper units for the in-process client — no DB. Build storage `Model`
//! / domain `Record` values by hand and assert the row→SDK-view projection,
//! the scale clamp, the unknown-enum fail-loud, and the OData error mapping.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::similar_names,
    clippy::redundant_closure,
    clippy::inconsistent_struct_constructor
)]

use std::str::FromStr;

use super::{
    ar_invoice_model_to_view, balance_model_to_view, entry_record_to_view, line_model_to_view,
    line_record_to_view, map_odata_page_err, parse_enum, scale_to_u8,
};
use crate::domain::model::{EntryRecord, LineRecord};
use crate::infra::storage::entity::{account_balance, ar_invoice_balance, journal_line};
use crate::infra::storage::repo::journal_repo::OdataPageError;
use bss_ledger_sdk::{AccountClass, MappingStatus, Side, SourceDocType};
use chrono::NaiveDate;
use time::OffsetDateTime;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// scale_to_u8
// ---------------------------------------------------------------------------

#[test]
fn scale_to_u8_passes_small_value() {
    assert_eq!(scale_to_u8(2), 2);
    assert_eq!(scale_to_u8(0), 0);
}

#[test]
fn scale_to_u8_clamps_negative_to_zero() {
    // Impossible-by-construction stored value must not panic.
    assert_eq!(scale_to_u8(-5), 0);
}

// ---------------------------------------------------------------------------
// parse_enum
// ---------------------------------------------------------------------------

#[test]
fn parse_enum_ok_known_literal() {
    let ok = parse_enum("DR", |s| Side::from_str(s)).expect("known literal parses");
    assert_eq!(ok, Side::Debit);
}

#[test]
fn parse_enum_unknown_literal_is_internal_500() {
    let err = parse_enum("NONSENSE", |s| Side::from_str(s)).expect_err("unknown is rejected");
    assert_eq!(err.status_code(), 500, "data corruption must fail loud");
}

// ---------------------------------------------------------------------------
// balance_model_to_view
// ---------------------------------------------------------------------------

fn sample_balance_row() -> account_balance::Model {
    account_balance::Model {
        tenant_id: Uuid::now_v7(),
        account_id: Uuid::now_v7(),
        currency: "USD".to_owned(),
        account_class: AccountClass::Revenue.as_str().to_owned(),
        normal_side: Side::Credit.as_str().to_owned(),
        balance_minor: 1000,
        functional_balance_minor: None,
        functional_currency: None,
        last_entry_seq: None,
        version: 1,
    }
}

#[test]
fn balance_model_to_view_projects_fields_and_parses_class() {
    let row = sample_balance_row();
    let view = balance_model_to_view(row).expect("valid class projects");
    assert_eq!(view.account_class, AccountClass::Revenue);
    assert_eq!(view.balance_minor, 1000);
    assert_eq!(view.currency, "USD");
}

#[test]
fn balance_model_to_view_unknown_class_fails_loud() {
    let mut row = sample_balance_row();
    row.account_class = "WAT".to_owned();
    assert_eq!(balance_model_to_view(row).unwrap_err().status_code(), 500);
}

// ---------------------------------------------------------------------------
// line_model_to_view
// ---------------------------------------------------------------------------

fn sample_journal_line_row() -> journal_line::Model {
    journal_line::Model {
        line_id: Uuid::now_v7(),
        entry_id: Uuid::now_v7(),
        tenant_id: Uuid::now_v7(),
        period_id: "2025-01".to_owned(),
        payer_tenant_id: Uuid::now_v7(),
        seller_tenant_id: None,
        resource_tenant_id: None,
        account_id: Uuid::now_v7(),
        account_class: AccountClass::Ar.as_str().to_owned(),
        gl_code: None,
        side: Side::Debit.as_str().to_owned(),
        amount_minor: 5000,
        currency: "EUR".to_owned(),
        currency_scale: 2,
        invoice_id: Some("INV-001".to_owned()),
        due_date: Some(NaiveDate::from_ymd_opt(2025, 12, 31).unwrap()),
        revenue_stream: None,
        mapping_status: MappingStatus::Resolved.as_str().to_owned(),
        functional_amount_minor: None,
        functional_currency: None,
        rate_snapshot_ref: None,
        tax_jurisdiction: None,
        tax_filing_period: None,
        tax_rate_ref: None,
        legal_entity_id: None,
        invoice_item_ref: None,
        sku_or_plan_ref: None,
        price_id: None,
        pricing_snapshot_ref: None,
        po_allocation_group: None,
        credit_grant_event_type: None,
        ar_status: None,
    }
}

#[test]
fn line_model_to_view_projects_fields() {
    let row = sample_journal_line_row();
    let view = line_model_to_view(row).expect("valid row projects");
    assert_eq!(view.account_class, AccountClass::Ar);
    assert_eq!(view.side, Side::Debit);
    assert_eq!(view.amount_minor, 5000);
    assert_eq!(view.currency, "EUR");
    assert_eq!(view.currency_scale, 2);
    assert_eq!(view.mapping_status, MappingStatus::Resolved);
    assert_eq!(view.invoice_id.as_deref(), Some("INV-001"));
}

#[test]
fn line_model_to_view_unknown_side_fails_loud() {
    let mut row = sample_journal_line_row();
    row.side = "NEITHER".to_owned();
    assert_eq!(line_model_to_view(row).unwrap_err().status_code(), 500);
}

#[test]
fn line_model_to_view_unknown_class_fails_loud() {
    let mut row = sample_journal_line_row();
    row.account_class = "BOGUS".to_owned();
    assert_eq!(line_model_to_view(row).unwrap_err().status_code(), 500);
}

#[test]
fn line_model_to_view_unknown_mapping_status_fails_loud() {
    let mut row = sample_journal_line_row();
    row.mapping_status = "UNKNOWN_STATUS".to_owned();
    assert_eq!(line_model_to_view(row).unwrap_err().status_code(), 500);
}

// ---------------------------------------------------------------------------
// ar_invoice_model_to_view  (infallible mapper, no enum parsing)
// ---------------------------------------------------------------------------

fn sample_ar_invoice_row() -> ar_invoice_balance::Model {
    ar_invoice_balance::Model {
        tenant_id: Uuid::now_v7(),
        payer_tenant_id: Uuid::now_v7(),
        account_id: Uuid::now_v7(),
        invoice_id: "INV-999".to_owned(),
        currency: "USD".to_owned(),
        balance_minor: 2500,
        disputed_minor: 0,
        functional_balance_minor: None,
        functional_currency: None,
        original_posted_at: Some(OffsetDateTime::now_utc()),
        due_date: Some(NaiveDate::from_ymd_opt(2026, 1, 31).unwrap()),
        last_entry_seq: Some(7),
        version: 3,
    }
}

#[test]
fn ar_invoice_model_to_view_projects_fields() {
    let row = sample_ar_invoice_row();
    let payer = row.payer_tenant_id;
    let account = row.account_id;
    let view = ar_invoice_model_to_view(row);
    assert_eq!(view.payer_tenant_id, payer);
    assert_eq!(view.account_id, account);
    assert_eq!(view.invoice_id, "INV-999");
    assert_eq!(view.currency, "USD");
    assert_eq!(view.balance_minor, 2500);
    assert!(view.due_date.is_some());
}

// ---------------------------------------------------------------------------
// line_record_to_view and entry_record_to_view
// ---------------------------------------------------------------------------

fn sample_line_record() -> LineRecord {
    LineRecord {
        line_id: Uuid::now_v7(),
        entry_id: Uuid::now_v7(),
        tenant_id: Uuid::now_v7(),
        period_id: "2025-01".to_owned(),
        payer_tenant_id: Uuid::now_v7(),
        seller_tenant_id: None,
        resource_tenant_id: None,
        account_id: Uuid::now_v7(),
        account_class: AccountClass::Revenue.as_str().to_owned(),
        gl_code: None,
        side: Side::Credit.as_str().to_owned(),
        amount_minor: 800,
        currency: "USD".to_owned(),
        currency_scale: 2,
        invoice_id: None,
        due_date: None,
        revenue_stream: Some("SaaS".to_owned()),
        mapping_status: MappingStatus::Resolved.as_str().to_owned(),
        functional_amount_minor: None,
        functional_currency: None,
        tax_jurisdiction: None,
        tax_filing_period: None,
        tax_rate_ref: None,
        legal_entity_id: None,
        invoice_item_ref: None,
        sku_or_plan_ref: None,
        price_id: None,
        pricing_snapshot_ref: None,
        po_allocation_group: None,
        credit_grant_event_type: None,
        ar_status: None,
    }
}

#[test]
fn line_record_to_view_projects_fields() {
    let rec = sample_line_record();
    let view = line_record_to_view(rec).expect("valid record projects");
    assert_eq!(view.account_class, AccountClass::Revenue);
    assert_eq!(view.side, Side::Credit);
    assert_eq!(view.amount_minor, 800);
    assert_eq!(view.currency, "USD");
    assert_eq!(view.currency_scale, 2);
    assert_eq!(view.revenue_stream.as_deref(), Some("SaaS"));
}

#[test]
fn line_record_to_view_unknown_side_fails_loud() {
    let mut rec = sample_line_record();
    rec.side = "SIDEWAYS".to_owned();
    assert_eq!(line_record_to_view(rec).unwrap_err().status_code(), 500);
}

fn sample_entry_record() -> EntryRecord {
    EntryRecord {
        entry_id: Uuid::now_v7(),
        tenant_id: Uuid::now_v7(),
        legal_entity_id: Uuid::now_v7(),
        period_id: "2025-01".to_owned(),
        entry_currency: "USD".to_owned(),
        source_doc_type: SourceDocType::InvoicePost.as_str().to_owned(),
        source_business_id: "BIZ-42".to_owned(),
        reverses_entry_id: None,
        reverses_period_id: None,
        posted_at_utc: OffsetDateTime::now_utc(),
        effective_at: NaiveDate::from_ymd_opt(2025, 1, 15).unwrap(),
        origin: "SYSTEM".to_owned(),
        posted_by_actor_id: Uuid::now_v7(),
        correlation_id: Uuid::now_v7(),
        rounding_evidence: serde_json::Value::Null,
        created_seq: 1,
        lines: vec![sample_line_record()],
    }
}

#[test]
fn entry_record_to_view_projects_header_fields() {
    let rec = sample_entry_record();
    let tenant = rec.tenant_id;
    let view = entry_record_to_view(rec).expect("valid record projects");
    assert_eq!(view.tenant_id, tenant);
    assert_eq!(view.source_doc_type, SourceDocType::InvoicePost);
    assert_eq!(view.source_business_id, "BIZ-42");
    assert_eq!(view.lines.len(), 1);
}

#[test]
fn entry_record_to_view_unknown_source_doc_type_fails_loud() {
    let mut rec = sample_entry_record();
    rec.source_doc_type = "ALIEN_DOC".to_owned();
    assert_eq!(entry_record_to_view(rec).unwrap_err().status_code(), 500);
}

#[test]
fn entry_record_to_view_unknown_line_enum_fails_loud() {
    let mut rec = sample_entry_record();
    rec.lines[0].account_class = "NOT_A_CLASS".to_owned();
    assert_eq!(entry_record_to_view(rec).unwrap_err().status_code(), 500);
}

// ---------------------------------------------------------------------------
// map_odata_page_err
// ---------------------------------------------------------------------------

#[test]
fn map_odata_page_err_db_is_internal_500_and_redacts() {
    let err = map_odata_page_err(OdataPageError::Db("secret-dsn-info".to_owned()));
    assert_eq!(err.status_code(), 500);
    let problem = toolkit::api::canonical_prelude::Problem::from(err);
    let body = serde_json::to_string(&problem).unwrap();
    assert!(
        !body.contains("secret-dsn-info"),
        "driver text must not leak: {body}"
    );
}

#[cfg(test)]
mod pg {
    //! testcontainers-postgres integration over the REAL `LedgerLocalClient`.
    //! The harness (`boot` / `authed_ctx` / `provision_req` / `balanced_post` /
    //! `account_ids` + the authz / seller fakes) is the shared foundation Tasks
    //! 8-11 call. The authz fakes + the authed-context builder are copied
    //! verbatim from `tests/rest_journal_entries.rs` (those live in the
    //! integration crate and can't be imported here); the seed helpers are
    //! copied from `tests/postgres_provisioning.rs`. One container per test.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::sync::Arc;

    use async_trait::async_trait;
    use authz_resolver_sdk::constraints::{Constraint, InPredicate, Predicate};
    use authz_resolver_sdk::models::{
        EvaluationRequest, EvaluationResponse, EvaluationResponseContext,
    };
    use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
    use bss_ledger_sdk::api::LedgerClientV1;
    use bss_ledger_sdk::{
        AccountClass, AllocateOutcome, AllocatePayment, FiscalCalendarSpec, Granularity,
        MappingStatus, ODataQuery, PostEntry, PostLine, ProvisionAccount, ProvisionCurrencyScale,
        ProvisionOutcome, ProvisionRequest, RecordDisputePhase, ReturnPayment,
        RevenueDisaggregationQuery, SettlePayment, Side, SourceDocType,
    };
    use chrono::NaiveDate;
    use sea_orm::Database;
    use sea_orm_migration::MigratorTrait;
    use testcontainers_modules::postgres::Postgres;
    use testcontainers_modules::testcontainers::runners::AsyncRunner;
    use toolkit::api::canonical_prelude::CanonicalError;
    use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
    use toolkit_gts::gts_id;
    use toolkit_security::{PlatformSecurityContext, SecurityContext};
    use uuid::Uuid;

    use crate::api::local_client::LedgerLocalClient;
    use crate::domain::ports::metrics::NoopLedgerMetrics;
    use crate::infra::events::publisher::LedgerEventPublisher;
    use crate::infra::seller_guard::{SellerGuard, TenantTypeReader};
    use crate::infra::storage::migrations::Migrator;

    // ── authz fakes (verbatim from tests/rest_journal_entries.rs) ──────────────
    fn subject_tenant_id(request: &EvaluationRequest) -> Uuid {
        request
            .subject
            .properties
            .get("tenant_id")
            .and_then(|v| v.as_str())
            .and_then(|s| Uuid::parse_str(s).ok())
            .unwrap_or_else(Uuid::nil)
    }

    fn tenant_in_constraint(tenant_id: Uuid) -> Constraint {
        Constraint {
            predicates: vec![Predicate::In(InPredicate::new(
                toolkit_security::pep_properties::OWNER_TENANT_ID,
                [tenant_id],
            ))],
        }
    }

    /// Always-allow fake that echoes the subject's tenant as an
    /// `owner_tenant_id` `In` constraint (the scope the read/write paths bind).
    pub(super) struct AllowAuthZ;

    #[async_trait]
    impl AuthZResolverApi for AllowAuthZ {
        async fn evaluate(
            &self,
            _ctx: PlatformSecurityContext,
            request: EvaluationRequest,
        ) -> Result<EvaluationResponse, CanonicalError> {
            let tenant_id = subject_tenant_id(&request);
            Ok(EvaluationResponse {
                decision: true,
                context: EvaluationResponseContext {
                    constraints: vec![tenant_in_constraint(tenant_id)],
                    deny_reason: None,
                },
            })
        }
    }

    /// Always-deny fake — models the PDP refusing the action so the gate maps it
    /// to a `PermissionDenied`. Held for Tasks 8-11 (the deny-path tests).
    #[allow(dead_code)]
    pub(super) struct DenyAuthZ;

    #[async_trait]
    impl AuthZResolverApi for DenyAuthZ {
        async fn evaluate(
            &self,
            _ctx: PlatformSecurityContext,
            _request: EvaluationRequest,
        ) -> Result<EvaluationResponse, CanonicalError> {
            Ok(EvaluationResponse {
                decision: false,
                context: EvaluationResponseContext {
                    constraints: vec![],
                    deny_reason: None,
                },
            })
        }
    }

    // ── seller-type fake (from infra/seller_guard_tests.rs) ────────────────────
    /// The tenant type the boot seller-guard is configured to accept; a
    /// [`FakeReader`] returning it lets `provision` clear the seller gate.
    pub(super) const SELLER_TYPE: &str =
        gts_id!("cf.core.am.tenant_type.v1~cf.bss.ledger.seller.v1~");

    /// Canned tenant-type reader (stands in for the AM `get_tenant` adapter).
    /// `Some(t)` resolves every tenant to type `t`; `None` resolves no type
    /// (drives the seller-guard `FailedPrecondition` path for Tasks 8-11).
    pub(super) struct FakeReader(pub Option<String>);

    #[async_trait]
    impl TenantTypeReader for FakeReader {
        async fn tenant_type(
            &self,
            _ctx: &SecurityContext,
            _tenant_id: Uuid,
        ) -> Result<Option<String>, toolkit::api::canonical_prelude::CanonicalError> {
            Ok(self.0.clone())
        }
    }

    /// An authenticated `SecurityContext` whose subject tenant is `tenant` —
    /// also the only tenant `AllowAuthZ` authorizes (its `In` constraint echoes
    /// the subject tenant), so reads/writes must target `tenant`.
    pub(super) fn authed_ctx(tenant: Uuid) -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::now_v7())
            .subject_tenant_id(tenant)
            .subject_type(gts_id!("cf.core.security.subject_user.v1~"))
            .token_scopes(vec!["*".to_owned()])
            .build()
            .expect("authed SecurityContext")
    }

    /// Boot PG, migrate, and build the real client with an ALLOW enforcer + a
    /// seller `FakeReader(Some(SELLER_TYPE))`. Returns the live container (keep
    /// it in scope for the test's duration), the client, and a fresh tenant id
    /// the caller seeds/authorizes against.
    pub(super) async fn boot() -> (
        testcontainers_modules::testcontainers::ContainerAsync<Postgres>,
        LedgerLocalClient,
        Uuid,
    ) {
        boot_with(Some(SELLER_TYPE.to_owned())).await
    }

    /// `boot` with the seller-type the `FakeReader` resolves made explicit:
    /// `Some(t)` clears the seller gate for type `t`; `None` makes `provision`
    /// fail the seller gate (`FailedPrecondition`). Tasks 8-11 use this to drive
    /// the non-seller / unresolved-type rejection paths.
    pub(super) async fn boot_with(
        seller_type: Option<String>,
    ) -> (
        testcontainers_modules::testcontainers::ContainerAsync<Postgres>,
        LedgerLocalClient,
        Uuid,
    ) {
        let container = test_containers::postgres().start().await.unwrap();
        let port = container.get_host_port_ipv4(5432).await.unwrap();
        let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

        // Raw sea-orm connection for the migrator.
        let raw = Database::connect(&url).await.unwrap();
        Migrator::up(&raw, None).await.unwrap();

        // The client connection sets search_path=bss (as the gear config does in
        // prod) so its unqualified entity queries resolve into the bss schema.
        let repo_url = format!("{url}?options=-c%20search_path%3Dbss,public");
        let tdb = connect_db(&repo_url, ConnectOpts::default()).await.unwrap();
        let provider = DBProvider::<DbError>::new(tdb);

        let publisher = Arc::new(LedgerEventPublisher::noop());
        let enforcer = Arc::new(PolicyEnforcer::new(Arc::new(AllowAuthZ)));
        let seller_guard = Arc::new(SellerGuard::new(
            Arc::new(FakeReader(seller_type)),
            [SELLER_TYPE.to_owned()],
        ));
        let metrics = Arc::new(NoopLedgerMetrics);
        let client = LedgerLocalClient::new(
            provider,
            publisher,
            enforcer,
            seller_guard,
            metrics,
            crate::config::FxConfig::default(),
            crate::config::PaymentsConfig::default(),
            crate::infra::period_close::CloseControlFeeds::inert(),
        );
        (container, client, Uuid::now_v7())
    }

    // ── seed helpers (from tests/postgres_provisioning.rs) ─────────────────────
    fn account(
        class: AccountClass,
        currency: &str,
        revenue_stream: Option<&str>,
        side: Side,
    ) -> ProvisionAccount {
        ProvisionAccount {
            account_class: class,
            currency: currency.to_owned(),
            revenue_stream: revenue_stream.map(str::to_owned),
            normal_side: side,
            may_go_negative: false,
        }
    }

    /// USD at scale 2 (ISO). Default headroom (`plausible_max_major` omitted).
    fn usd2_scale() -> ProvisionCurrencyScale {
        ProvisionCurrencyScale {
            currency: "USD".to_owned(),
            minor_units: 2,
            plausible_max_major: None,
            source: "iso".to_owned(),
        }
    }

    fn utc_calendar() -> FiscalCalendarSpec {
        FiscalCalendarSpec {
            timezone: "UTC".to_owned(),
            granularity: Granularity::Month,
            fy_start_month: 1,
            functional_currency: None,
        }
    }

    /// Seller-shaped provision request for `tenant`: AR plus
    /// REVENUE(subscription) plus TAX_PAYABLE accounts, USD@2, and a UTC monthly
    /// calendar. `provision` seeds the OPEN period for the CURRENT month and
    /// returns its id in `ProvisionOutcome::period_id` — build the post against
    /// that id, never a hard-coded period.
    pub(super) fn provision_req(tenant: Uuid) -> ProvisionRequest {
        ProvisionRequest {
            tenant_id: tenant,
            accounts: vec![
                account(AccountClass::Ar, "USD", None, Side::Debit),
                account(
                    AccountClass::Revenue,
                    "USD",
                    Some("subscription"),
                    Side::Credit,
                ),
                account(AccountClass::TaxPayable, "USD", None, Side::Credit),
            ],
            currency_scales: vec![usd2_scale()],
            fiscal_calendar: utc_calendar(),
        }
    }

    /// A payment-shaped provision for `tenant`: the four money-flow chart
    /// accounts (CASH_CLEARING / UNALLOCATED / PSP_FEE_EXPENSE / AR) plus USD@2
    /// and a UTC monthly calendar. `provision` seeds the OPEN period for the
    /// CURRENT month (settle/allocate derive `period_id` from `OffsetDateTime::now_utc()`).
    pub(super) fn payment_provision_req(tenant: Uuid) -> ProvisionRequest {
        ProvisionRequest {
            tenant_id: tenant,
            accounts: vec![
                account(AccountClass::CashClearing, "USD", None, Side::Debit),
                account(AccountClass::Unallocated, "USD", None, Side::Credit),
                account(AccountClass::PspFeeExpense, "USD", None, Side::Debit),
                account(AccountClass::Ar, "USD", None, Side::Debit),
            ],
            currency_scales: vec![usd2_scale()],
            fiscal_calendar: utc_calendar(),
        }
    }

    /// `(ar, revenue, tax)` `account_id`s from a [`ProvisionOutcome`], by class.
    pub(super) fn account_ids(out: &ProvisionOutcome) -> (Uuid, Uuid, Uuid) {
        let pick = |c: AccountClass| {
            out.accounts
                .iter()
                .find(|a| a.account_class == c)
                .expect("provisioned account")
                .account_id
        };
        (
            pick(AccountClass::Ar),
            pick(AccountClass::Revenue),
            pick(AccountClass::TaxPayable),
        )
    }

    /// A balanced invoice post for `tenant` into `period_id` against the
    /// provisioned chart: DR AR 1200 / CR REVENUE 1000 / CR TAX_PAYABLE 200.
    /// `ar` / `rev` / `tax` are the `account_id`s from the prior `provision`
    /// outcome (see [`account_ids`]); `payer` is the AR payer tenant.
    pub(super) fn balanced_post(
        tenant: Uuid,
        payer: Uuid,
        period_id: &str,
        ar: Uuid,
        rev: Uuid,
        tax: Uuid,
    ) -> PostEntry {
        let line = |account_id,
                    class,
                    side,
                    amount,
                    invoice: Option<&str>,
                    rstream: Option<&str>,
                    taxj: Option<&str>| PostLine {
            line_id: Uuid::now_v7(),
            payer_tenant_id: payer,
            seller_tenant_id: None,
            resource_tenant_id: None,
            account_id,
            account_class: class,
            gl_code: None,
            side,
            amount_minor: amount,
            currency: "USD".to_owned(),
            invoice_id: invoice.map(str::to_owned),
            due_date: invoice.map(|_| NaiveDate::from_ymd_opt(2026, 7, 1).unwrap()),
            revenue_stream: rstream.map(str::to_owned),
            mapping_status: MappingStatus::Resolved,
            functional_amount_minor: None,
            functional_currency: None,
            tax_jurisdiction: taxj.map(str::to_owned),
            tax_filing_period: taxj.map(|_| "2026Q3".to_owned()),
            tax_rate_ref: None,
            invoice_item_ref: None,
            sku_or_plan_ref: None,
            price_id: None,
            pricing_snapshot_ref: None,
            po_allocation_group: None,
            credit_grant_event_type: None,
            ar_status: None,
        };
        PostEntry {
            entry_id: Uuid::now_v7(),
            tenant_id: tenant,
            period_id: period_id.to_owned(),
            entry_currency: "USD".to_owned(),
            source_doc_type: SourceDocType::InvoicePost,
            source_business_id: "INV-1".to_owned(),
            effective_at: NaiveDate::from_ymd_opt(2026, 6, 1).unwrap(),
            posted_by_actor_id: Uuid::now_v7(),
            correlation_id: Uuid::now_v7(),
            reverses_entry_id: None,
            reverses_period_id: None,
            lines: vec![
                line(
                    ar,
                    AccountClass::Ar,
                    Side::Debit,
                    1200,
                    Some("INV-1"),
                    None,
                    None,
                ),
                line(
                    rev,
                    AccountClass::Revenue,
                    Side::Credit,
                    1000,
                    None,
                    Some("subscription"),
                    None,
                ),
                line(
                    tax,
                    AccountClass::TaxPayable,
                    Side::Credit,
                    200,
                    None,
                    None,
                    Some("US-CA"),
                ),
            ],
        }
    }

    /// Boot with a DENY enforcer — the PEP refuses every action → PermissionDenied.
    pub(super) async fn boot_deny() -> (
        testcontainers_modules::testcontainers::ContainerAsync<Postgres>,
        LedgerLocalClient,
        Uuid,
    ) {
        let container = test_containers::postgres().start().await.unwrap();
        let port = container.get_host_port_ipv4(5432).await.unwrap();
        let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

        let raw = Database::connect(&url).await.unwrap();
        Migrator::up(&raw, None).await.unwrap();

        let repo_url = format!("{url}?options=-c%20search_path%3Dbss,public");
        let tdb = connect_db(&repo_url, ConnectOpts::default()).await.unwrap();
        let provider = DBProvider::<DbError>::new(tdb);

        let publisher = Arc::new(LedgerEventPublisher::noop());
        let enforcer = Arc::new(PolicyEnforcer::new(Arc::new(DenyAuthZ)));
        let seller_guard = Arc::new(SellerGuard::new(
            Arc::new(FakeReader(Some(SELLER_TYPE.to_owned()))),
            [SELLER_TYPE.to_owned()],
        ));
        let metrics = Arc::new(NoopLedgerMetrics);
        let client = LedgerLocalClient::new(
            provider,
            publisher,
            enforcer,
            seller_guard,
            metrics,
            crate::config::FxConfig::default(),
            crate::config::PaymentsConfig::default(),
            crate::infra::period_close::CloseControlFeeds::inert(),
        );
        (container, client, Uuid::now_v7())
    }

    /// Boot with an explicit `FakeReader` — use for non-seller gate tests.
    pub(super) async fn boot_with_reader(
        reader: FakeReader,
    ) -> (
        testcontainers_modules::testcontainers::ContainerAsync<Postgres>,
        LedgerLocalClient,
        Uuid,
    ) {
        let container = test_containers::postgres().start().await.unwrap();
        let port = container.get_host_port_ipv4(5432).await.unwrap();
        let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

        let raw = Database::connect(&url).await.unwrap();
        Migrator::up(&raw, None).await.unwrap();

        let repo_url = format!("{url}?options=-c%20search_path%3Dbss,public");
        let tdb = connect_db(&repo_url, ConnectOpts::default()).await.unwrap();
        let provider = DBProvider::<DbError>::new(tdb);

        let publisher = Arc::new(LedgerEventPublisher::noop());
        let enforcer = Arc::new(PolicyEnforcer::new(Arc::new(AllowAuthZ)));
        let seller_guard = Arc::new(SellerGuard::new(Arc::new(reader), [SELLER_TYPE.to_owned()]));
        let metrics = Arc::new(NoopLedgerMetrics);
        let client = LedgerLocalClient::new(
            provider,
            publisher,
            enforcer,
            seller_guard,
            metrics,
            crate::config::FxConfig::default(),
            crate::config::PaymentsConfig::default(),
            crate::infra::period_close::CloseControlFeeds::inert(),
        );
        (container, client, Uuid::now_v7())
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn post_balanced_entry_happy_path() {
        let (_c, client, tenant) = boot().await;
        let ctx = authed_ctx(tenant);

        // Seed the chart + OPEN period, then post a balanced invoice entry into
        // the period `provision` opened (CURRENT month — derive it, don't hard
        // code, so the test is stable across months).
        let out = client
            .provision(&ctx, provision_req(tenant))
            .await
            .expect("provision must succeed");
        let (ar, rev, tax) = account_ids(&out);

        let entry = balanced_post(tenant, Uuid::now_v7(), &out.period_id, ar, rev, tax);
        let want = entry.entry_id;

        let posted = client
            .post_balanced_entry(&ctx, entry)
            .await
            .expect("balanced post must succeed");
        assert!(!posted.replayed, "first post is not a replay");
        assert_eq!(posted.entry_id, want, "post returns the supplied entry id");
    }

    // ── Task 8: post deny + unbalanced ──────────────────────────────────────────

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn post_denied_maps_to_permission_denied() {
        let (_c, client, tenant) = boot_deny().await;
        // Deny short-circuits before any DB read, so unprovisioned ids are fine.
        let entry = balanced_post(
            tenant,
            Uuid::now_v7(),
            "202606",
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
        );
        let err = client
            .post_balanced_entry(&authed_ctx(tenant), entry)
            .await
            .unwrap_err();
        assert_eq!(err.status_code(), 403);
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn post_unbalanced_maps_to_invalid_argument() {
        let (_c, client, tenant) = boot().await;
        let ctx = authed_ctx(tenant);
        let out = client.provision(&ctx, provision_req(tenant)).await.unwrap();
        let (ar, rev, tax) = account_ids(&out);
        let mut entry = balanced_post(tenant, Uuid::now_v7(), &out.period_id, ar, rev, tax);
        // DR 1199 ≠ CR 1200 → unbalanced
        entry.lines[0].amount_minor = 1199;
        let err = client.post_balanced_entry(&ctx, entry).await.unwrap_err();
        assert_eq!(err.status_code(), 400);
    }

    // ── Task 9: reads + cross-tenant BOLA ───────────────────────────────────────

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn reads_return_seeded_ledger_and_are_tenant_scoped() {
        let (_c, client, tenant) = boot().await;
        let ctx = authed_ctx(tenant);
        let out = client.provision(&ctx, provision_req(tenant)).await.unwrap();
        let (ar, rev, tax) = account_ids(&out);
        let entry = balanced_post(tenant, Uuid::now_v7(), &out.period_id, ar, rev, tax);
        let entry_id = entry.entry_id;
        client.post_balanced_entry(&ctx, entry).await.unwrap();

        // Owner reads see the seeded ledger.
        assert!(
            client
                .get_entry(&ctx, tenant, entry_id)
                .await
                .unwrap()
                .is_some(),
            "owner should see their entry"
        );
        assert!(
            client
                .read_account_balance(&ctx, tenant, ar)
                .await
                .unwrap()
                .is_some(),
            "owner should see AR balance"
        );
        assert!(
            !client
                .list_lines(&ctx, tenant, &ODataQuery::default())
                .await
                .unwrap()
                .items
                .is_empty(),
            "owner should see lines"
        );
        assert!(
            !client
                .list_balances(&ctx, tenant, &ODataQuery::default())
                .await
                .unwrap()
                .items
                .is_empty(),
            "owner should see balances"
        );
        assert!(
            !client
                .list_ar_invoice_balances(&ctx, tenant, None)
                .await
                .unwrap()
                .is_empty(),
            "owner should see AR invoice balances"
        );
        assert!(
            !client
                .list_accounts(&ctx, tenant, &ODataQuery::default())
                .await
                .unwrap()
                .items
                .is_empty(),
            "owner should see accounts"
        );

        // A FOREIGN tenant ctx is SQL-scoped out → empty, never a 403 (BOLA).
        let other = authed_ctx(Uuid::now_v7());
        assert!(
            client
                .get_entry(&other, tenant, entry_id)
                .await
                .unwrap()
                .is_none(),
            "foreign ctx must not see other tenant's entry (BOLA)"
        );
        assert!(
            client
                .list_lines(&other, tenant, &ODataQuery::default())
                .await
                .unwrap()
                .items
                .is_empty(),
            "foreign ctx must not see other tenant's lines (BOLA)"
        );
    }

    // ── Task 10: close_period ───────────────────────────────────────────────────

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn close_period_transitions_open_to_closed() {
        let (_c, client, tenant) = boot().await;
        let ctx = authed_ctx(tenant);
        let out = client.provision(&ctx, provision_req(tenant)).await.unwrap();
        let outcome = client
            .close_period(&ctx, tenant, out.period_id.clone())
            .await
            .unwrap();
        assert_eq!(outcome.period_id, out.period_id, "period_id echoed");
        assert!(!outcome.already_closed, "first close is not already_closed");

        // A post into the now-closed period must fail 400 (PERIOD_CLOSED).
        let (ar, rev, tax) = account_ids(&out);
        let entry = balanced_post(tenant, Uuid::now_v7(), &out.period_id, ar, rev, tax);
        let err = client.post_balanced_entry(&ctx, entry).await.unwrap_err();
        assert_eq!(
            err.status_code(),
            400,
            "posting into a closed period must be 400"
        );
    }

    // ── Task 11: provision allow / deny / non-seller ────────────────────────────

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn provision_seller_succeeds() {
        let (_c, client, tenant) = boot().await;
        let out = client
            .provision(&authed_ctx(tenant), provision_req(tenant))
            .await
            .unwrap();
        assert!(
            out.accounts_created >= 3 || out.accounts_existing >= 3,
            "chart must be seeded with at least 3 accounts"
        );
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn provision_denied_is_403() {
        let (_c, client, tenant) = boot_deny().await;
        let err = client
            .provision(&authed_ctx(tenant), provision_req(tenant))
            .await
            .unwrap_err();
        assert_eq!(err.status_code(), 403);
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn provision_non_seller_is_failed_precondition() {
        let (_c, client, tenant) =
            boot_with_reader(FakeReader(Some("buyer-type".to_owned()))).await;
        let err = client
            .provision(&authed_ctx(tenant), provision_req(tenant))
            .await
            .unwrap_err();
        // FailedPrecondition: TENANT_TYPE_NOT_LEDGER_OWNER → 400
        assert_eq!(err.status_code(), 400);
    }

    // ── Payment money-in / money-out through the client ─────────────────────────

    /// The payment flow through the in-process client end-to-end: `settle_payment`
    /// parks a receipt in the unallocated pool, `read_unallocated` reads it back as
    /// an `UnallocatedView`, `allocate_payment` (precedence mode, `splits = None`)
    /// drains it onto an open AR invoice, and `list_payment_allocations` returns the
    /// persisted `payment_allocation` rows as `AllocationView`s. Exercises the four
    /// payment client methods + the `caller_splits = None` mapping arm of
    /// `allocate_payment`, the `AllocationApplied`/`AllocationView` shaping, the
    /// `UnallocatedView` shaping, and `payment_allocation_to_view` — the
    /// in-process surface other gears call, none of it reachable from the
    /// out-of-crate tests (`LedgerLocalClient::new` is `pub(crate)`).
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn settle_read_allocate_list_through_client() {
        let (_c, client, tenant) = boot().await;
        let ctx = authed_ctx(tenant);
        let payer = Uuid::now_v7();

        // Provision the payment chart (CASH/UNALLOCATED/PSP_FEE/AR) + the OPEN
        // current-month period settle/allocate derive.
        let out = client
            .provision(&ctx, payment_provision_req(tenant))
            .await
            .expect("payment provision must succeed");
        let ar = out
            .accounts
            .iter()
            .find(|a| a.account_class == AccountClass::Ar)
            .expect("AR provisioned")
            .account_id;
        let psp = out
            .accounts
            .iter()
            .find(|a| a.account_class == AccountClass::PspFeeExpense)
            .expect("PSP_FEE provisioned")
            .account_id;

        // Settle gross=1000 fee=0 ⇒ the whole gross parks in UNALLOCATED.
        let settled = client
            .settle_payment(
                &ctx,
                SettlePayment {
                    tenant_id: tenant,
                    payer_tenant_id: payer,
                    payment_id: "PAY-LC-1".to_owned(),
                    gross_minor: 1000,
                    fee_minor: 0,
                    currency: "USD".to_owned(),
                    scale: 2,
                    effective_at: None,
                },
            )
            .await
            .expect("settle must succeed");
        assert!(!settled.replayed, "first settle is fresh");

        // read_unallocated returns the pooled gross as an UnallocatedView.
        let pool = client
            .read_unallocated(&ctx, tenant, payer, "USD".to_owned())
            .await
            .expect("read unallocated");
        assert_eq!(pool.balance_minor, 1000, "gross parked in UNALLOCATED");
        assert_eq!(pool.payer_tenant_id, payer);
        assert_eq!(pool.currency, "USD");

        // Seed an open AR invoice (DR AR 400 / CR PSP_FEE 400 — PSP_FEE is
        // unguarded, so the CR from zero is allowed) into the same period.
        let inv = PostEntry {
            entry_id: Uuid::now_v7(),
            tenant_id: tenant,
            period_id: out.period_id.clone(),
            entry_currency: "USD".to_owned(),
            source_doc_type: SourceDocType::InvoicePost,
            source_business_id: "INV-LC".to_owned(),
            effective_at: NaiveDate::from_ymd_opt(2026, 6, 1).unwrap(),
            posted_by_actor_id: Uuid::now_v7(),
            correlation_id: Uuid::now_v7(),
            reverses_entry_id: None,
            reverses_period_id: None,
            lines: vec![
                PostLine {
                    line_id: Uuid::now_v7(),
                    payer_tenant_id: payer,
                    seller_tenant_id: Some(tenant),
                    resource_tenant_id: None,
                    account_id: ar,
                    account_class: AccountClass::Ar,
                    gl_code: None,
                    side: Side::Debit,
                    amount_minor: 400,
                    currency: "USD".to_owned(),
                    invoice_id: Some("INV-LC".to_owned()),
                    due_date: Some(NaiveDate::from_ymd_opt(2026, 12, 1).unwrap()),
                    revenue_stream: None,
                    mapping_status: MappingStatus::Resolved,
                    functional_amount_minor: None,
                    functional_currency: None,
                    tax_jurisdiction: None,
                    tax_filing_period: None,
                    tax_rate_ref: None,
                    invoice_item_ref: None,
                    sku_or_plan_ref: None,
                    price_id: None,
                    pricing_snapshot_ref: None,
                    po_allocation_group: None,
                    credit_grant_event_type: None,
                    ar_status: None,
                },
                PostLine {
                    line_id: Uuid::now_v7(),
                    account_id: psp,
                    account_class: AccountClass::PspFeeExpense,
                    side: Side::Credit,
                    invoice_id: None,
                    due_date: None,
                    payer_tenant_id: payer,
                    seller_tenant_id: Some(tenant),
                    resource_tenant_id: None,
                    gl_code: None,
                    amount_minor: 400,
                    currency: "USD".to_owned(),
                    revenue_stream: None,
                    mapping_status: MappingStatus::Resolved,
                    functional_amount_minor: None,
                    functional_currency: None,
                    tax_jurisdiction: None,
                    tax_filing_period: None,
                    tax_rate_ref: None,
                    invoice_item_ref: None,
                    sku_or_plan_ref: None,
                    price_id: None,
                    pricing_snapshot_ref: None,
                    po_allocation_group: None,
                    credit_grant_event_type: None,
                    ar_status: None,
                },
            ],
        };
        client
            .post_balanced_entry(&ctx, inv)
            .await
            .expect("seed AR invoice");

        // Allocate 400 (precedence mode) ⇒ inline-posted onto INV-LC.
        let outcome = client
            .allocate_payment(
                &ctx,
                AllocatePayment {
                    tenant_id: tenant,
                    payer_tenant_id: payer,
                    payment_id: "PAY-LC-1".to_owned(),
                    allocation_id: Uuid::now_v7(),
                    lump_minor: 400,
                    currency: "USD".to_owned(),
                    scale: 2,
                    hint_invoice_id: None,
                    splits: None,
                },
            )
            .await
            .expect("allocate must succeed");
        let applied = match outcome {
            AllocateOutcome::Applied(a) => a,
            AllocateOutcome::Queued(q) => panic!("expected an inline allocation, got {q:?}"),
        };
        assert!(!applied.posting.replayed, "first allocate is fresh");
        assert_eq!(applied.allocations.len(), 1, "one invoice filled");
        assert_eq!(applied.allocations[0].invoice_id, "INV-LC");
        assert_eq!(applied.allocations[0].amount_minor, 400);

        // list_payment_allocations returns the persisted row as an AllocationView.
        let listed = client
            .list_payment_allocations(&ctx, tenant, "PAY-LC-1".to_owned())
            .await
            .expect("list allocations");
        assert_eq!(listed.len(), 1, "one persisted allocation row");
        assert_eq!(listed[0].invoice_id, "INV-LC");
        assert_eq!(listed[0].amount_minor, 400);
        assert_eq!(listed[0].currency, "USD");

        // The pool drained by the allocated total (1000 - 400 = 600 left).
        let pool_after = client
            .read_unallocated(&ctx, tenant, payer, "USD".to_owned())
            .await
            .expect("read unallocated after allocate");
        assert_eq!(pool_after.balance_minor, 600, "pool drained by 400");
    }

    /// `record_dispute_phase` with an unknown `phase` literal is rejected
    /// `InvalidArgument` (400) at the boundary parse — BEFORE any post — exercising
    /// the `DisputePhase::parse(...) None` arm of the client (a bad wire literal is
    /// a 400, not a deep post-path fault). The authz gate passes (subject tenant =
    /// target), so the parse is what trips.
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn record_dispute_phase_bad_literal_is_400() {
        let (_c, client, tenant) = boot().await;
        let ctx = authed_ctx(tenant);

        let err = client
            .record_dispute_phase(
                &ctx,
                RecordDisputePhase {
                    tenant_id: tenant,
                    payer_tenant_id: Uuid::now_v7(),
                    payment_id: "PAY-D".to_owned(),
                    dispute_id: "D-1".to_owned(),
                    invoice_id: None,
                    cycle: 1,
                    phase: "not-a-phase".to_owned(),
                    funds_at_open: "withheld".to_owned(),
                    disputed_amount_minor: 100,
                    currency: "USD".to_owned(),
                    scale: 2,
                    effective_at: None,
                },
            )
            .await
            .expect_err("an unknown dispute phase must be rejected");
        assert_eq!(
            err.status_code(),
            400,
            "a bad phase literal is InvalidArgument (400)"
        );
    }

    /// The recognition read plane through the client: a freshly-provisioned tenant
    /// with no schedules returns empty lists / `None`, exercising
    /// `read_recognition_scope` (a DIFFERENT scope from the `entry` data plane the
    /// other reads use) + the repo delegation + the empty-mapping arms of
    /// `list_recognition_schedules`, `get_recognition_schedule`, and
    /// `list_revenue_disaggregation`. (A populated recognition ledger is covered by
    /// the dedicated recognition suites; this pins the client's recognition-scope
    /// wiring + the empty read shape.)
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn recognition_reads_empty_on_fresh_tenant() {
        let (_c, client, tenant) = boot().await;
        let ctx = authed_ctx(tenant);
        client
            .provision(&ctx, provision_req(tenant))
            .await
            .expect("provision");

        let schedules = client
            .list_recognition_schedules(&ctx, tenant, None, None)
            .await
            .expect("list recognition schedules");
        assert!(schedules.schedules.is_empty(), "no schedules yet");
        assert!(!schedules.truncated, "an empty list is not truncated");

        let one = client
            .get_recognition_schedule(&ctx, tenant, "no-such-schedule".to_owned())
            .await
            .expect("get recognition schedule");
        assert!(one.is_none(), "an absent schedule resolves to None");

        let disagg = client
            .list_revenue_disaggregation(
                &ctx,
                RevenueDisaggregationQuery {
                    tenant_id: tenant,
                    period_id: None,
                },
            )
            .await
            .expect("list revenue disaggregation");
        assert!(disagg.entries.is_empty(), "no recognized revenue yet");
    }

    // ── Full-flow integration (real client + real Postgres; only the PDP +
    //    tenant-type reader are fakes). NOT the project e2e suite — those are the
    //    pytest black-box tests under testing/e2e/suites/ledger/. ──

    /// Full ledger flow: provision → post → read → reverse → tie-out.
    /// Drives the real `LedgerLocalClient` against a real database: provision a
    /// seller, post a balanced invoice, read it back and see the AR balance,
    /// post its reversal, watch every balance net to zero, and confirm the books
    /// tie out (zero variance). Boots inline (not via `boot`) so it can KEEP the
    /// provider for the closing `TieOutJob` — `boot` moves it into the client.
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn provision_post_read_reverse_ties_out() {
        let container = test_containers::postgres().start().await.unwrap();
        let port = container.get_host_port_ipv4(5432).await.unwrap();
        let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
        let raw = Database::connect(&url).await.unwrap();
        Migrator::up(&raw, None).await.unwrap();
        let repo_url = format!("{url}?options=-c%20search_path%3Dbss,public");
        let tdb = connect_db(&repo_url, ConnectOpts::default()).await.unwrap();
        let provider = DBProvider::<DbError>::new(tdb);
        let publisher = Arc::new(LedgerEventPublisher::noop());
        let client = LedgerLocalClient::new(
            provider.clone(),
            Arc::clone(&publisher),
            Arc::new(PolicyEnforcer::new(Arc::new(AllowAuthZ))),
            Arc::new(SellerGuard::new(
                Arc::new(FakeReader(Some(SELLER_TYPE.to_owned()))),
                [SELLER_TYPE.to_owned()],
            )),
            Arc::new(NoopLedgerMetrics),
            crate::config::FxConfig::default(),
            crate::config::PaymentsConfig::default(),
            crate::infra::period_close::CloseControlFeeds::inert(),
        );
        let tenant = Uuid::now_v7();
        let ctx = authed_ctx(tenant);
        let payer = Uuid::now_v7();

        // provision → post (DR AR 1200 / CR REVENUE 1000 / CR TAX 200).
        let out = client
            .provision(&ctx, provision_req(tenant))
            .await
            .expect("provision");
        let (ar, rev, tax) = account_ids(&out);
        let entry = balanced_post(tenant, payer, &out.period_id, ar, rev, tax);
        let entry_id = entry.entry_id;
        client.post_balanced_entry(&ctx, entry).await.expect("post");

        // read: the entry is visible and AR reflects the DR 1200.
        assert!(
            client
                .get_entry(&ctx, tenant, entry_id)
                .await
                .expect("get_entry")
                .is_some(),
            "posted entry must be readable"
        );
        assert_eq!(
            client
                .read_account_balance(&ctx, tenant, ar)
                .await
                .expect("read AR"),
            Some(1200),
            "AR reflects the posted debit"
        );

        // reverse: flip every side; the header points back at the original.
        let mut reversal = balanced_post(tenant, payer, &out.period_id, ar, rev, tax);
        reversal.source_doc_type = SourceDocType::Reversal;
        reversal.source_business_id = "reverses=INV-1".to_owned();
        reversal.reverses_entry_id = Some(entry_id);
        reversal.reverses_period_id = Some(out.period_id.clone());
        for l in &mut reversal.lines {
            l.side = match l.side {
                Side::Debit => Side::Credit,
                Side::Credit => Side::Debit,
            };
        }
        client
            .post_balanced_entry(&ctx, reversal)
            .await
            .expect("reversal post");

        // every balance nets back to zero.
        for (label, acct) in [("AR", ar), ("REVENUE", rev), ("TAX", tax)] {
            assert_eq!(
                client
                    .read_account_balance(&ctx, tenant, acct)
                    .await
                    .expect("read balance"),
                Some(0),
                "{label} nets to zero after the reversal"
            );
        }

        // tie-out over the SAME database: recompute every grain from journal_line
        // and confirm zero variance (clean books close the loop).
        let report = crate::infra::jobs::tieout::TieOutJob::new(provider, publisher)
            .tie_out_tenant(tenant)
            .await
            .expect("tie-out runs");
        assert!(
            report.is_clean(),
            "books must tie out after post+reverse: {}",
            report.summary()
        );
    }

    /// Payments E2E through the real client: provision the money-flow chart,
    /// settle a payment, and see the net land in the payer's unallocated pool;
    /// a replay of the same `payment_id` is idempotent (no double-credit).
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn settle_lands_in_unallocated_and_replays() {
        let (_c, client, tenant) = boot().await;
        let ctx = authed_ctx(tenant);
        client
            .provision(&ctx, payment_provision_req(tenant))
            .await
            .expect("provision payment chart");

        let payer = Uuid::now_v7();
        let settle = SettlePayment {
            tenant_id: tenant,
            payer_tenant_id: payer,
            payment_id: "PAY-1".to_owned(),
            gross_minor: 1000,
            fee_minor: 0,
            currency: "USD".to_owned(),
            scale: 2,
            effective_at: None,
        };

        let first = client
            .settle_payment(&ctx, settle.clone())
            .await
            .expect("settle");
        assert!(!first.replayed, "first settle is a fresh post");

        // The net (gross − fee) lands in the payer's unallocated pool.
        let pool = client
            .read_unallocated(&ctx, tenant, payer, "USD".to_owned())
            .await
            .expect("read unallocated");
        assert_eq!(pool.balance_minor, 1000, "settled net sits in unallocated");

        // A replay of the same payment is idempotent — same ref, no double-credit.
        let replay = client
            .settle_payment(&ctx, settle)
            .await
            .expect("settle replay");
        assert!(replay.replayed, "same payment_id replays");
        let pool_after = client
            .read_unallocated(&ctx, tenant, payer, "USD".to_owned())
            .await
            .expect("read unallocated after replay");
        assert_eq!(
            pool_after.balance_minor, 1000,
            "replay must not double-credit the pool"
        );
    }

    /// Money-in lifecycle E2E: provision → post an invoice (opens AR) → settle a
    /// payment (funds the pool) → allocate the pool onto the open invoice. Asserts
    /// the pool drains to zero, the invoice's AR is paid off, and an allocation
    /// row is recorded. Exercises the allocation path — including the
    /// touched-invoice size bound — end-to-end through the real client.
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn settle_allocate_drains_pool_into_ar() {
        let (_c, client, tenant) = boot().await;
        let ctx = authed_ctx(tenant);
        let payer = Uuid::now_v7();

        // A combined chart: the invoice classes (AR / REVENUE / TAX) plus the
        // money-flow classes (CASH_CLEARING / UNALLOCATED / PSP_FEE_EXPENSE).
        let req = ProvisionRequest {
            tenant_id: tenant,
            accounts: vec![
                account(AccountClass::Ar, "USD", None, Side::Debit),
                account(
                    AccountClass::Revenue,
                    "USD",
                    Some("subscription"),
                    Side::Credit,
                ),
                account(AccountClass::TaxPayable, "USD", None, Side::Credit),
                account(AccountClass::CashClearing, "USD", None, Side::Debit),
                account(AccountClass::Unallocated, "USD", None, Side::Credit),
                account(AccountClass::PspFeeExpense, "USD", None, Side::Debit),
            ],
            currency_scales: vec![usd2_scale()],
            fiscal_calendar: utc_calendar(),
        };
        let out = client.provision(&ctx, req).await.expect("provision");
        let (ar, rev, tax) = account_ids(&out);

        // Post an invoice for `payer`: DR AR 1200 (INV-1) / CR REVENUE 1000 / CR TAX 200.
        let invoice = balanced_post(tenant, payer, &out.period_id, ar, rev, tax);
        client
            .post_balanced_entry(&ctx, invoice)
            .await
            .expect("invoice post");

        // Settle a 1200 receipt for the same payer → pool holds 1200.
        client
            .settle_payment(
                &ctx,
                SettlePayment {
                    tenant_id: tenant,
                    payer_tenant_id: payer,
                    payment_id: "PAY-1".to_owned(),
                    gross_minor: 1200,
                    fee_minor: 0,
                    currency: "USD".to_owned(),
                    scale: 2,
                    effective_at: None,
                },
            )
            .await
            .expect("settle");

        // Allocate the pool onto the open invoice (settled ⇒ applies inline).
        let outcome = client
            .allocate_payment(
                &ctx,
                AllocatePayment {
                    tenant_id: tenant,
                    payer_tenant_id: payer,
                    payment_id: "PAY-1".to_owned(),
                    allocation_id: Uuid::now_v7(),
                    lump_minor: 1200,
                    currency: "USD".to_owned(),
                    scale: 2,
                    hint_invoice_id: None,
                    splits: None,
                },
            )
            .await
            .expect("allocate");
        assert!(
            matches!(outcome, AllocateOutcome::Applied(_)),
            "a settled payment allocates inline"
        );

        // The pool is fully drained, the invoice's AR is paid off, and the
        // allocation is recorded.
        assert_eq!(
            client
                .read_unallocated(&ctx, tenant, payer, "USD".to_owned())
                .await
                .expect("read unallocated")
                .balance_minor,
            0,
            "the pool drains into AR"
        );
        assert_eq!(
            client
                .read_account_balance(&ctx, tenant, ar)
                .await
                .expect("read AR"),
            Some(0),
            "the invoice's AR is paid off"
        );
        assert!(
            !client
                .list_payment_allocations(&ctx, tenant, "PAY-1".to_owned())
                .await
                .expect("list allocations")
                .is_empty(),
            "the allocation is recorded"
        );
    }

    /// Settlement-return E2E: settle a 1000 receipt, then record a PSP return of
    /// 400 that claws part of it back out of the unallocated pool. Asserts the
    /// pool drops by exactly the returned amount, and the return replays
    /// idempotently on `psp_return_id` (no second claw-back).
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn settlement_return_claws_back_pool() {
        let (_c, client, tenant) = boot().await;
        let ctx = authed_ctx(tenant);
        client
            .provision(&ctx, payment_provision_req(tenant))
            .await
            .expect("provision payment chart");
        let payer = Uuid::now_v7();

        client
            .settle_payment(
                &ctx,
                SettlePayment {
                    tenant_id: tenant,
                    payer_tenant_id: payer,
                    payment_id: "PAY-1".to_owned(),
                    gross_minor: 1000,
                    fee_minor: 0,
                    currency: "USD".to_owned(),
                    scale: 2,
                    effective_at: None,
                },
            )
            .await
            .expect("settle");

        // A partial return of 400 (leaves settled_minor positive, so a replay can
        // re-size its fee share and short-circuit on the dedup).
        let ret = ReturnPayment {
            tenant_id: tenant,
            payer_tenant_id: payer,
            payment_id: "PAY-1".to_owned(),
            psp_return_id: "RET-1".to_owned(),
            amount_minor: 400,
            currency: "USD".to_owned(),
            scale: 2,
            effective_at: None,
        };
        client
            .return_payment(&ctx, ret.clone())
            .await
            .expect("return");
        assert_eq!(
            client
                .read_unallocated(&ctx, tenant, payer, "USD".to_owned())
                .await
                .expect("read unallocated")
                .balance_minor,
            600,
            "the return claws 400 back out of the 1000 pool"
        );

        // Idempotent on psp_return_id — a replay is a no-op on the pool.
        let replay = client
            .return_payment(&ctx, ret)
            .await
            .expect("return replay");
        assert!(replay.replayed, "same psp_return_id replays");
        assert_eq!(
            client
                .read_unallocated(&ctx, tenant, payer, "USD".to_owned())
                .await
                .expect("read unallocated after replay")
                .balance_minor,
            600,
            "replay must not claw back a second time"
        );
    }
}
