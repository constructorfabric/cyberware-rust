//! API-level (router) tests for the journal-entry / balance REST surface:
//! `POST /journal-entries` (target tenant in the body),
//! `POST /journal-entries/{entryId}/reversals` (tenant from context),
//! and the read endpoints (`GET /balances`).
//!
//! Drives the router via `tower::ServiceExt::oneshot` against a stub
//! `LedgerClientV1` + a stub `InvoicePoster` (no DB) and an in-test
//! `PolicyEnforcer` fake (no PDP). Covers the post happy path (201, allow +
//! auth), the unauthenticated path (401 problem+json), a malformed body (400
//! problem+json), a PDP deny (403 problem+json), a reverse-of-a-reversal
//! (400 problem+json carrying `CANNOT_REVERSE_REVERSAL` — the domain rejects
//! before the post path), and a balances read (200 with the stub rows).
//!
//! It also covers the read/idempotency contracts (Task F1): an idempotent
//! replay renders `200` (not `201`) carrying the prior posting ref;
//! `GET /journal-lines` passes the cursor page through (items + `next_cursor`);
//! `GET /balances/ar-aging` returns the bucketed AR shape; and
//! `GET /journal-entries/{entryId}` carries the audit who/when/source dims
//! (`posted_by_actor_id` / `posted_at_utc` / `origin` / `source_doc_type` /
//! `source_business_id` / `correlation_id`, AC #8).

#![allow(
    clippy::non_ascii_literal,
    clippy::let_underscore_must_use,
    clippy::needless_collect,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::doc_markdown,
    clippy::panic
)]

use std::sync::Arc;

use async_trait::async_trait;
use authz_resolver_sdk::constraints::{Constraint, InPredicate, Predicate};
use authz_resolver_sdk::models::{
    EvaluationRequest, EvaluationResponse, EvaluationResponseContext,
};
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use bss_ledger::api::rest::journal_entries::{ApiState, router};
use bss_ledger::domain::error::DomainError;
use bss_ledger::domain::instant::to_naive_date;
use bss_ledger::domain::invoice::builder::PostedInvoice;
use bss_ledger::infra::invoice_post::InvoicePoster;
use bss_ledger_sdk::api::LedgerClientV1;
use bss_ledger_sdk::posting::{
    ArInvoiceBalanceView, BalanceView, EntryView, LineView, ODataQuery, Page, PostEntry, PostingRef,
};
use bss_ledger_sdk::{ProvisionOutcome, ProvisionRequest, SourceDocType};
use time::OffsetDateTime;
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit_db::secure::AccessScope;
use toolkit_gts::gts_id;
use toolkit_odata::PageInfo;
use toolkit_security::PlatformSecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

/// The canned posted/replayed entry id the stub poster + `get_entry` return.
const STUB_ENTRY: Uuid = uuid::uuid!("dddddddd-dddd-dddd-dddd-dddddddddddd");

/// A terminal `PageInfo` (no further pages): the shape a single-page stub read
/// hands back. The canonical `Page` envelope always carries one.
fn complete_page_info() -> PageInfo {
    PageInfo {
        next_cursor: None,
        prev_cursor: None,
        limit: 100,
    }
}

/// Pull a `payer_tenant_id eq <uuid>` out of the parsed `$filter` AST so a stub
/// can prove the OData filter threaded through from the wire. Returns `None`
/// when the filter is absent or not that exact equality shape.
fn payer_from_filter(query: &ODataQuery) -> Option<Uuid> {
    use toolkit_odata::ast::{CompareOperator, Expr, Value};
    match query.filter()? {
        Expr::Compare(lhs, CompareOperator::Eq, rhs) => match (&**lhs, &**rhs) {
            (Expr::Identifier(name), Expr::Value(Value::Uuid(u))) if name == "payer_tenant_id" => {
                Some(*u)
            }
            _ => None,
        },
        _ => None,
    }
}

// ── Stubs ────────────────────────────────────────────────────────────────────

/// In-test data-access stub. `get_entry` returns a `REVERSAL`-typed view (so a
/// reverse-of-it is rejected by the domain) and `list_balances` returns one
/// canned row; the other methods are not exercised by these router tests.
struct StubClient;

#[async_trait::async_trait]
impl LedgerClientV1 for StubClient {
    async fn return_payment(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _req: bss_ledger_sdk::ReturnPayment,
    ) -> Result<bss_ledger_sdk::PostingRef, CanonicalError> {
        unimplemented!("not exercised by these router tests")
    }

    async fn record_dispute_phase(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _req: bss_ledger_sdk::RecordDisputePhase,
    ) -> Result<bss_ledger_sdk::DisputeOutcome, CanonicalError> {
        unimplemented!("not exercised by these router tests")
    }

    async fn post_credit_application(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _req: bss_ledger_sdk::CreditApplication,
    ) -> Result<bss_ledger_sdk::CreditApplicationApplied, CanonicalError> {
        unimplemented!("not exercised by the journal-entry router tests")
    }

    async fn post_balanced_entry(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _entry: PostEntry,
    ) -> Result<PostingRef, CanonicalError> {
        unimplemented!("posts go through the InvoicePoster, not the client")
    }

    async fn read_account_balance(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _tenant_id: Uuid,
        _account_id: Uuid,
    ) -> Result<Option<i64>, CanonicalError> {
        unimplemented!("not exercised by the journal router tests")
    }

    async fn list_accounts(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _tenant_id: Uuid,
        _query: &ODataQuery,
    ) -> Result<Page<bss_ledger_sdk::AccountInfo>, CanonicalError> {
        unimplemented!("not exercised by the journal router tests")
    }

    async fn get_entry(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        tenant_id: Uuid,
        entry_id: Uuid,
    ) -> Result<Option<EntryView>, CanonicalError> {
        // A REVERSAL-typed view so the reverse-of-a-reversal test trips
        // CannotReverseReversal before the post path.
        Ok(Some(EntryView {
            entry_id,
            tenant_id,
            period_id: "202606".to_owned(),
            entry_currency: "USD".to_owned(),
            source_doc_type: SourceDocType::Reversal,
            source_business_id: "reverses=00000000-0000-0000-0000-000000000001".to_owned(),
            reverses_entry_id: Some(uuid::uuid!("00000000-0000-0000-0000-000000000001")),
            reverses_period_id: Some("202606".to_owned()),
            posted_at_utc: OffsetDateTime::now_utc(),
            effective_at: to_naive_date(OffsetDateTime::now_utc()),
            posted_by_actor_id: tenant_id,
            origin: "SYSTEM".to_owned(),
            correlation_id: tenant_id,
            created_seq: 1,
            lines: Vec::new(),
        }))
    }

    async fn list_lines(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _tenant_id: Uuid,
        _query: &ODataQuery,
    ) -> Result<Page<LineView>, CanonicalError> {
        unimplemented!("not exercised by the journal router tests")
    }

    async fn list_balances(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _tenant_id: Uuid,
        _query: &ODataQuery,
    ) -> Result<Page<BalanceView>, CanonicalError> {
        Ok(Page {
            items: vec![BalanceView {
                account_id: uuid::uuid!("99999999-9999-9999-9999-999999999999"),
                account_class: bss_ledger_sdk::AccountClass::Ar,
                currency: "USD".to_owned(),
                balance_minor: 1200,
                functional_balance_minor: None,
                functional_currency: None,
            }],
            page_info: complete_page_info(),
        })
    }

    async fn list_ar_invoice_balances(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _tenant_id: Uuid,
        _payer_tenant_id: Option<Uuid>,
    ) -> Result<Vec<ArInvoiceBalanceView>, CanonicalError> {
        unimplemented!("not exercised by the journal router tests")
    }

    async fn provision(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _req: ProvisionRequest,
    ) -> Result<ProvisionOutcome, CanonicalError> {
        unimplemented!("not exercised by the journal router tests")
    }

    async fn close_period(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _tenant_id: Uuid,
        _period_id: String,
    ) -> Result<bss_ledger_sdk::CloseOutcome, CanonicalError> {
        unimplemented!("not exercised by the journal router tests")
    }

    async fn settle_payment(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _req: bss_ledger_sdk::SettlePayment,
    ) -> Result<PostingRef, CanonicalError> {
        unimplemented!("not exercised by the journal router tests")
    }

    async fn allocate_payment(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _req: bss_ledger_sdk::AllocatePayment,
    ) -> Result<bss_ledger_sdk::AllocateOutcome, CanonicalError> {
        unimplemented!("not exercised by the journal router tests")
    }

    async fn list_payment_allocations(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _tenant_id: Uuid,
        _payment_id: String,
    ) -> Result<Vec<bss_ledger_sdk::AllocationView>, CanonicalError> {
        unimplemented!("not exercised by the journal router tests")
    }

    async fn read_unallocated(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _tenant_id: Uuid,
        _payer_tenant_id: Uuid,
        _currency: String,
    ) -> Result<bss_ledger_sdk::UnallocatedView, CanonicalError> {
        unimplemented!("not exercised by the journal router tests")
    }

    async fn trigger_recognition_run(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _req: bss_ledger_sdk::TriggerRecognitionRun,
    ) -> Result<bss_ledger_sdk::RecognitionRunOutcome, CanonicalError> {
        unimplemented!("not exercised by the journal router tests")
    }

    async fn list_revenue_disaggregation(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _query: bss_ledger_sdk::RevenueDisaggregationQuery,
    ) -> Result<bss_ledger_sdk::RevenueDisaggregation, CanonicalError> {
        unimplemented!("not exercised by the journal router tests")
    }

    async fn change_recognition_schedule(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _cmd: bss_ledger_sdk::ChangeRecognitionSchedule,
    ) -> Result<bss_ledger_sdk::ScheduleChangeRef, CanonicalError> {
        unimplemented!("not exercised by the journal router tests")
    }

    async fn get_recognition_schedule(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _tenant_id: Uuid,
        _schedule_id: String,
    ) -> Result<Option<bss_ledger_sdk::RecognitionScheduleView>, CanonicalError> {
        unimplemented!("not exercised by the journal router tests")
    }

    async fn list_recognition_schedules(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _tenant_id: Uuid,
        _invoice_id: Option<String>,
        _revenue_stream: Option<String>,
    ) -> Result<bss_ledger_sdk::RecognitionScheduleList, CanonicalError> {
        // The post path calls this when an invoice carries recognition; the stub
        // posts no schedules, so an empty list is the faithful response.
        Ok(bss_ledger_sdk::RecognitionScheduleList::default())
    }
}

/// In-test write stub: a fresh post returns a non-replayed reference (so the
/// handler renders 201); the reversal post is never reached in these tests.
struct StubPoster;

#[async_trait]
impl InvoicePoster for StubPoster {
    async fn post_invoice(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _scope: &AccessScope,
        _inv: &PostedInvoice,
        _payer_open: bool,
    ) -> Result<PostingRef, DomainError> {
        Ok(PostingRef {
            entry_id: STUB_ENTRY,
            created_seq: 1,
            replayed: false,
        })
    }

    async fn post_reversal(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _scope: &AccessScope,
        _reversal: PostEntry,
        _reason: Option<String>,
    ) -> Result<PostingRef, DomainError> {
        unimplemented!("the reverse-of-a-reversal test rejects before the post")
    }

    async fn post_correction(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _scope: &AccessScope,
        _correction: PostEntry,
    ) -> Result<PostingRef, DomainError> {
        unimplemented!("mapping-correction is not exercised by the router tests")
    }
}

/// In-test annotation write stub: the router tests are DB-free, and no test
/// here exercises the annotation route, so this just satisfies the port.
struct StubAnnotationWriter;

#[async_trait]
impl bss_ledger::infra::annotation::AnnotationWriter for StubAnnotationWriter {
    async fn set(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _scope: &AccessScope,
        _tenant: Uuid,
        _target_id: Uuid,
        _target_period_id: String,
        _target: bss_ledger::infra::annotation::AnnotationTarget,
        _description: Option<String>,
        _actor_ref: String,
        _reason: Option<String>,
        _correlation_id: Option<Uuid>,
    ) -> Result<(), DomainError> {
        Ok(())
    }
}

/// Write stub that captures the `posted_by_actor_id` the handler lowered onto
/// the domain `PostedInvoice` — so a test can prove the actor was stamped from
/// the authenticated subject server-side (not read from the request body).
struct CapturingPoster {
    seen_actor: Arc<std::sync::Mutex<Option<Uuid>>>,
}

#[async_trait]
impl InvoicePoster for CapturingPoster {
    async fn post_invoice(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _scope: &AccessScope,
        inv: &PostedInvoice,
        _payer_open: bool,
    ) -> Result<PostingRef, DomainError> {
        *self.seen_actor.lock().expect("lock") = Some(inv.posted_by_actor_id);
        Ok(PostingRef {
            entry_id: STUB_ENTRY,
            created_seq: 1,
            replayed: false,
        })
    }

    async fn post_reversal(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _scope: &AccessScope,
        _reversal: PostEntry,
        _reason: Option<String>,
    ) -> Result<PostingRef, DomainError> {
        unimplemented!("not exercised by the actor-stamping test")
    }

    async fn post_correction(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _scope: &AccessScope,
        _correction: PostEntry,
    ) -> Result<PostingRef, DomainError> {
        unimplemented!("not exercised by the actor-stamping test")
    }
}

/// Write stub whose `post_invoice` reports an idempotent REPLAY (`replayed:
/// true`) of a prior post — so the handler must render `200`, not `201`, with
/// the prior posting reference. The reversal / correction posts are not reached.
struct ReplayPoster;

#[async_trait]
impl InvoicePoster for ReplayPoster {
    async fn post_invoice(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _scope: &AccessScope,
        _inv: &PostedInvoice,
        _payer_open: bool,
    ) -> Result<PostingRef, DomainError> {
        Ok(PostingRef {
            entry_id: STUB_ENTRY,
            created_seq: 7,
            replayed: true,
        })
    }

    async fn post_reversal(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _scope: &AccessScope,
        _reversal: PostEntry,
        _reason: Option<String>,
    ) -> Result<PostingRef, DomainError> {
        unimplemented!("not exercised by the replay test")
    }

    async fn post_correction(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _scope: &AccessScope,
        _correction: PostEntry,
    ) -> Result<PostingRef, DomainError> {
        unimplemented!("not exercised by the replay test")
    }
}

/// Read stub for the page / aging / audit contracts. `list_lines` returns a
/// non-terminal page (one line + a `next_cursor`); `list_ar_invoice_balances`
/// returns two past-due AR rows; `get_entry` returns an `INVOICE_POST` view
/// stamped with the audit who/when/source dims. The other reads are unused.
struct ReadStubClient;

/// The canned audit dims `ReadStubClient::get_entry` stamps (asserted by the
/// who/when/source test).
const AUDIT_ACTOR: Uuid = uuid::uuid!("a0a0a0a0-a0a0-a0a0-a0a0-a0a0a0a0a0a0");
const AUDIT_CORRELATION: Uuid = uuid::uuid!("c0c0c0c0-c0c0-c0c0-c0c0-c0c0c0c0c0c0");
const AUDIT_BUSINESS_ID: &str = "INV-AUDIT-1";
const AUDIT_ORIGIN: &str = "SYSTEM";
/// The next-page cursor `ReadStubClient::list_lines` hands back (asserted by the
/// pagination passthrough test).
const NEXT_CURSOR: &str = "cursor-page-2";
/// The payer the AR-aging rows belong to (asserted by the aging-shape test).
const AGING_PAYER: Uuid = uuid::uuid!("b0b0b0b0-b0b0-b0b0-b0b0-b0b0b0b0b0b0");

#[async_trait::async_trait]
impl LedgerClientV1 for ReadStubClient {
    async fn return_payment(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _req: bss_ledger_sdk::ReturnPayment,
    ) -> Result<bss_ledger_sdk::PostingRef, CanonicalError> {
        unimplemented!("not exercised by these router tests")
    }

    async fn record_dispute_phase(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _req: bss_ledger_sdk::RecordDisputePhase,
    ) -> Result<bss_ledger_sdk::DisputeOutcome, CanonicalError> {
        unimplemented!("not exercised by these router tests")
    }

    async fn post_credit_application(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _req: bss_ledger_sdk::CreditApplication,
    ) -> Result<bss_ledger_sdk::CreditApplicationApplied, CanonicalError> {
        unimplemented!("not exercised by the journal-entry router tests")
    }

    async fn post_balanced_entry(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _entry: PostEntry,
    ) -> Result<PostingRef, CanonicalError> {
        unimplemented!("posts go through the InvoicePoster, not the client")
    }

    async fn read_account_balance(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _tenant_id: Uuid,
        _account_id: Uuid,
    ) -> Result<Option<i64>, CanonicalError> {
        unimplemented!("not exercised by the read-contract tests")
    }

    async fn list_accounts(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _tenant_id: Uuid,
        _query: &ODataQuery,
    ) -> Result<Page<bss_ledger_sdk::AccountInfo>, CanonicalError> {
        unimplemented!("not exercised by the read-contract tests")
    }

    async fn get_entry(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        tenant_id: Uuid,
        entry_id: Uuid,
    ) -> Result<Option<EntryView>, CanonicalError> {
        // An INVOICE_POST view carrying the audit who/when/source dims AC #8
        // requires the read DTO to expose.
        Ok(Some(EntryView {
            entry_id,
            tenant_id,
            period_id: "202606".to_owned(),
            entry_currency: "USD".to_owned(),
            source_doc_type: SourceDocType::InvoicePost,
            source_business_id: AUDIT_BUSINESS_ID.to_owned(),
            reverses_entry_id: None,
            reverses_period_id: None,
            posted_at_utc: OffsetDateTime::now_utc(),
            effective_at: to_naive_date(OffsetDateTime::now_utc()),
            posted_by_actor_id: AUDIT_ACTOR,
            origin: AUDIT_ORIGIN.to_owned(),
            correlation_id: AUDIT_CORRELATION,
            created_seq: 42,
            lines: Vec::new(),
        }))
    }

    async fn list_lines(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        tenant_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<LineView>, CanonicalError> {
        // A non-terminal page: one line + a `next_cursor` in `page_info` (so the
        // handler must surface BOTH the items and the continuation token). The
        // line echoes the `$filter`'s `payer_tenant_id eq <uuid>` so the OData
        // passthrough is observable on the wire.
        let payer = payer_from_filter(query).unwrap_or(tenant_id);
        Ok(Page {
            items: vec![LineView {
                line_id: uuid::uuid!("1e1e1e1e-1e1e-1e1e-1e1e-1e1e1e1e1e1e"),
                entry_id: STUB_ENTRY,
                payer_tenant_id: payer,
                account_id: uuid::uuid!("acacacac-acac-acac-acac-acacacacacac"),
                account_class: bss_ledger_sdk::AccountClass::Ar,
                gl_code: None,
                side: bss_ledger_sdk::Side::Debit,
                amount_minor: 1200,
                currency: "USD".to_owned(),
                currency_scale: 2,
                invoice_id: Some("INV-1".to_owned()),
                due_date: Some(to_naive_date(OffsetDateTime::now_utc())),
                revenue_stream: None,
                mapping_status: bss_ledger_sdk::MappingStatus::Resolved,
                functional_amount_minor: None,
                functional_currency: None,
                tax_jurisdiction: None,
                tax_filing_period: None,
                ar_status: None,
            }],
            page_info: PageInfo {
                next_cursor: Some(NEXT_CURSOR.to_owned()),
                prev_cursor: None,
                limit: 1,
            },
        })
    }

    async fn list_balances(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _tenant_id: Uuid,
        _query: &ODataQuery,
    ) -> Result<Page<BalanceView>, CanonicalError> {
        unimplemented!("not exercised by the read-contract tests")
    }

    async fn list_ar_invoice_balances(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _tenant_id: Uuid,
        _payer_tenant_id: Option<Uuid>,
    ) -> Result<Vec<ArInvoiceBalanceView>, CanonicalError> {
        // Two open AR invoices for one payer: one ~45 days past due (1-30? no —
        // 31-60) and one ~10 days past due (1-30). The handler folds these into
        // the domain aging buckets; the test asserts the bucketed shape.
        let today = to_naive_date(OffsetDateTime::now_utc());
        Ok(vec![
            ArInvoiceBalanceView {
                payer_tenant_id: AGING_PAYER,
                account_id: uuid::uuid!("a4a4a4a4-a4a4-a4a4-a4a4-a4a4a4a4a4a4"),
                invoice_id: "INV-OLD".to_owned(),
                currency: "USD".to_owned(),
                balance_minor: 5000,
                due_date: Some(today - chrono::Duration::days(45)),
            },
            ArInvoiceBalanceView {
                payer_tenant_id: AGING_PAYER,
                account_id: uuid::uuid!("a4a4a4a4-a4a4-a4a4-a4a4-a4a4a4a4a4a4"),
                invoice_id: "INV-NEW".to_owned(),
                currency: "USD".to_owned(),
                balance_minor: 3000,
                due_date: Some(today - chrono::Duration::days(10)),
            },
        ])
    }

    async fn provision(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _req: ProvisionRequest,
    ) -> Result<ProvisionOutcome, CanonicalError> {
        unimplemented!("not exercised by the read-contract tests")
    }

    async fn close_period(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _tenant_id: Uuid,
        _period_id: String,
    ) -> Result<bss_ledger_sdk::CloseOutcome, CanonicalError> {
        unimplemented!("not exercised by the read-contract tests")
    }

    async fn settle_payment(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _req: bss_ledger_sdk::SettlePayment,
    ) -> Result<PostingRef, CanonicalError> {
        unimplemented!("not exercised by the read-contract tests")
    }

    async fn allocate_payment(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _req: bss_ledger_sdk::AllocatePayment,
    ) -> Result<bss_ledger_sdk::AllocateOutcome, CanonicalError> {
        unimplemented!("not exercised by the read-contract tests")
    }

    async fn list_payment_allocations(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _tenant_id: Uuid,
        _payment_id: String,
    ) -> Result<Vec<bss_ledger_sdk::AllocationView>, CanonicalError> {
        unimplemented!("not exercised by the read-contract tests")
    }

    async fn read_unallocated(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _tenant_id: Uuid,
        _payer_tenant_id: Uuid,
        _currency: String,
    ) -> Result<bss_ledger_sdk::UnallocatedView, CanonicalError> {
        unimplemented!("not exercised by the read-contract tests")
    }

    async fn trigger_recognition_run(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _req: bss_ledger_sdk::TriggerRecognitionRun,
    ) -> Result<bss_ledger_sdk::RecognitionRunOutcome, CanonicalError> {
        unimplemented!("not exercised by the read-contract tests")
    }

    async fn list_revenue_disaggregation(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _query: bss_ledger_sdk::RevenueDisaggregationQuery,
    ) -> Result<bss_ledger_sdk::RevenueDisaggregation, CanonicalError> {
        unimplemented!("not exercised by the read-contract tests")
    }

    async fn change_recognition_schedule(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _cmd: bss_ledger_sdk::ChangeRecognitionSchedule,
    ) -> Result<bss_ledger_sdk::ScheduleChangeRef, CanonicalError> {
        unimplemented!("not exercised by the read-contract tests")
    }

    async fn get_recognition_schedule(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _tenant_id: Uuid,
        _schedule_id: String,
    ) -> Result<Option<bss_ledger_sdk::RecognitionScheduleView>, CanonicalError> {
        unimplemented!("not exercised by the read-contract tests")
    }

    async fn list_recognition_schedules(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _tenant_id: Uuid,
        _invoice_id: Option<String>,
        _revenue_stream: Option<String>,
    ) -> Result<bss_ledger_sdk::RecognitionScheduleList, CanonicalError> {
        // The post path calls this when an invoice carries recognition; the stub
        // posts no schedules, so an empty list is the faithful response.
        Ok(bss_ledger_sdk::RecognitionScheduleList::default())
    }
}

// ── Authz fakes (mirror rest_provisioning.rs) ────────────────────────────────

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

/// Always-allow fake that echoes the subject's tenant as an `owner_tenant_id`
/// `In` constraint.
struct AllowAuthZ;

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

/// Always-deny fake — models the PDP refusing the action so the gate maps it to
/// 403.
struct DenyAuthZ;

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

fn allow_enforcer() -> PolicyEnforcer {
    PolicyEnforcer::new(Arc::new(AllowAuthZ))
}

fn deny_enforcer() -> PolicyEnforcer {
    PolicyEnforcer::new(Arc::new(DenyAuthZ))
}

/// Build the journal router with the stubs and the supplied enforcer (layered
/// as an `Extension`, as the production `register_rest` does).
fn router_with_enforcer(enforcer: PolicyEnforcer) -> Router {
    let state = Arc::new(ApiState {
        client: Arc::new(StubClient) as Arc<dyn LedgerClientV1>,
        posting: Arc::new(StubPoster) as Arc<dyn InvoicePoster>,
        approval: None,
        annotation: Arc::new(StubAnnotationWriter)
            as Arc<dyn bss_ledger::infra::annotation::AnnotationWriter>,
        journal_repo: None,
        posting_policy: None,
    });
    let openapi = toolkit::api::OpenApiRegistryImpl::new();
    router(state, &openapi).layer(axum::Extension(enforcer))
}

fn base_router() -> Router {
    router_with_enforcer(allow_enforcer())
}

/// Build the journal router with explicit client + poster stubs and the
/// always-allow enforcer. Lets a test swap in a `ReplayPoster` / `ReadStubClient`
/// without disturbing the default-stub router the other tests use.
fn router_with_stubs(client: Arc<dyn LedgerClientV1>, posting: Arc<dyn InvoicePoster>) -> Router {
    let state = Arc::new(ApiState {
        client,
        posting,
        approval: None,
        annotation: Arc::new(StubAnnotationWriter)
            as Arc<dyn bss_ledger::infra::annotation::AnnotationWriter>,
        journal_repo: None,
        posting_policy: None,
    });
    let openapi = toolkit::api::OpenApiRegistryImpl::new();
    router(state, &openapi).layer(axum::Extension(allow_enforcer()))
}

/// The authenticated caller's tenant — also the only tenant the allow fake
/// authorizes (its `In` constraint echoes it).
const SUBJECT_TENANT: Uuid = uuid::uuid!("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");

/// The authenticated caller's subject id (the actor the handler must stamp onto
/// the post, regardless of any body field).
const SUBJECT_ID: Uuid = uuid::uuid!("11111111-2222-3333-4444-555555555555");

fn authed_context() -> toolkit_security::SecurityContext {
    toolkit_security::SecurityContext::builder()
        .subject_id(SUBJECT_ID)
        .subject_tenant_id(SUBJECT_TENANT)
        .subject_type(gts_id!("cf.core.security.subject_user.v1~"))
        .token_scopes(vec!["*".to_owned()])
        .build()
        .expect("authed SecurityContext must build")
}

/// A valid snake_case post-invoice body (target tenant = `SUBJECT_TENANT`).
fn valid_post_body() -> serde_json::Value {
    serde_json::json!({
        "tenant_id": SUBJECT_TENANT,
        "invoice_id": "INV-1",
        "payer_tenant_id": "22222222-2222-2222-2222-222222222222",
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
        "correlation_id": "11111111-2222-3333-4444-555555555555"
    })
}

fn post_uri() -> String {
    "/bss-ledger/v1/journal-entries".to_owned()
}

fn problem_content_type(response: &axum::http::Response<Body>) -> String {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn post_invoice_happy_path_returns_201() {
    let router = base_router().layer(axum::Extension(authed_context()));
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(post_uri())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(valid_post_body().to_string()))
                .expect("build req"),
        )
        .await
        .expect("send");

    assert_eq!(response.status(), StatusCode::CREATED);
    let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("body must be JSON");
    assert_eq!(
        value["entry_id"],
        serde_json::json!("dddddddd-dddd-dddd-dddd-dddddddddddd")
    );
    assert_eq!(value["replayed"], serde_json::json!(false));
}

/// The handler stamps `posted_by_actor_id` from the AUTHENTICATED subject, never
/// from the request body (which no longer carries it): the capturing poster sees
/// the authed subject id, proving the actor is server-side, not caller-forgeable.
#[tokio::test]
async fn post_invoice_stamps_actor_from_authenticated_subject() {
    let seen_actor = Arc::new(std::sync::Mutex::new(None));
    let poster = Arc::new(CapturingPoster {
        seen_actor: Arc::clone(&seen_actor),
    });
    let router = router_with_stubs(
        Arc::new(StubClient) as Arc<dyn LedgerClientV1>,
        poster as Arc<dyn InvoicePoster>,
    )
    .layer(axum::Extension(authed_context()));
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(post_uri())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(valid_post_body().to_string()))
                .expect("build req"),
        )
        .await
        .expect("send");

    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(
        *seen_actor.lock().expect("lock"),
        Some(SUBJECT_ID),
        "the handler must stamp the authenticated subject as the poster"
    );
}

#[tokio::test]
async fn post_invoice_without_auth_returns_401() {
    // No Extension(ctx) layer => require_authenticated fails with 401 (the
    // enforcer IS layered, so it is not a missing-extension 500).
    let router = base_router();
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(post_uri())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(valid_post_body().to_string()))
                .expect("build req"),
        )
        .await
        .expect("send");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let ct = problem_content_type(&response);
    assert!(
        ct.contains("application/problem+json"),
        "expected problem+json, got '{ct}'"
    );
}

#[tokio::test]
async fn post_invoice_malformed_body_returns_400() {
    let router = base_router().layer(axum::Extension(authed_context()));
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(post_uri())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{"))
                .expect("build req"),
        )
        .await
        .expect("send");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let ct = problem_content_type(&response);
    assert!(
        ct.contains("application/problem+json"),
        "expected problem+json, got '{ct}'"
    );
    let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("body must be JSON");
    assert!(
        value.to_string().contains("json_syntax_error"),
        "expected the json_syntax_error reason code; got {value}"
    );
}

#[tokio::test]
async fn post_invoice_denied_returns_403() {
    let router = router_with_enforcer(deny_enforcer()).layer(axum::Extension(authed_context()));
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(post_uri())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(valid_post_body().to_string()))
                .expect("build req"),
        )
        .await
        .expect("send");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let ct = problem_content_type(&response);
    assert!(
        ct.contains("application/problem+json"),
        "expected problem+json, got '{ct}'"
    );
}

#[tokio::test]
async fn reverse_of_a_reversal_returns_400_cannot_reverse_reversal() {
    // The stub `get_entry` returns a REVERSAL-typed view, so `build_reversal`
    // rejects with CannotReverseReversal BEFORE the post path — a 400 carrying
    // the CANNOT_REVERSE_REVERSAL reason.
    let entry = uuid::uuid!("cccccccc-cccc-cccc-cccc-cccccccccccc");
    let router = base_router().layer(axum::Extension(authed_context()));
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/bss-ledger/v1/journal-entries/{entry}/reversals"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "reason": "oops" }).to_string(),
                ))
                .expect("build req"),
        )
        .await
        .expect("send");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let ct = problem_content_type(&response);
    assert!(
        ct.contains("application/problem+json"),
        "expected problem+json, got '{ct}'"
    );
    let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("body must be JSON");
    assert!(
        value.to_string().contains("CANNOT_REVERSE_REVERSAL"),
        "expected the CANNOT_REVERSE_REVERSAL reason; got {value}"
    );
}

#[tokio::test]
async fn list_balances_returns_stub_rows() {
    let router = base_router().layer(axum::Extension(authed_context()));
    let response = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/bss-ledger/v1/balances")
                .body(Body::empty())
                .expect("build req"),
        )
        .await
        .expect("send");

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("body must be JSON");
    // Canonical OData `Page` envelope: rows under `items`, cursor metadata under
    // `page_info` (no more bespoke `{ balances: [...] }`).
    assert_eq!(
        value["items"][0]["account_id"],
        serde_json::json!("99999999-9999-9999-9999-999999999999")
    );
    assert_eq!(value["items"][0]["balance_minor"], serde_json::json!(1200));
    assert_eq!(value["items"][0]["account_class"], serde_json::json!("AR"));
    assert!(
        value["page_info"].is_object(),
        "the Page envelope must carry page_info, got {value}"
    );
}

/// Idempotent replay contrast to `post_invoice_happy_path_returns_201`: when the
/// `InvoicePoster` reports `replayed: true` (a re-post of an already-posted
/// invoice), the handler renders `200 OK`, NOT `201 Created`, carrying the prior
/// posting reference — the read/idempotency contract callers rely on to tell a
/// fresh post from a replay.
#[tokio::test]
async fn post_invoice_replay_returns_200_with_prior_ref() {
    let router = router_with_stubs(
        Arc::new(StubClient) as Arc<dyn LedgerClientV1>,
        Arc::new(ReplayPoster) as Arc<dyn InvoicePoster>,
    )
    .layer(axum::Extension(authed_context()));
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(post_uri())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(valid_post_body().to_string()))
                .expect("build req"),
        )
        .await
        .expect("send");

    // 200 (replay), not 201 (fresh).
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("body must be JSON");
    assert_eq!(
        value["entry_id"],
        serde_json::json!("dddddddd-dddd-dddd-dddd-dddddddddddd"),
        "the replay returns the prior posting ref"
    );
    assert_eq!(
        value["replayed"],
        serde_json::json!(true),
        "the body marks the post a replay"
    );
    assert_eq!(value["created_seq"], serde_json::json!(7));
}

/// `GET /journal-lines` threads the OData `$filter` through and surfaces the
/// canonical `Page` envelope: the stub `list_lines` echoes the
/// `$filter=payer_tenant_id eq <uuid>` onto the returned line (proving the
/// filter reached the client) and returns a non-terminal page whose
/// `page_info.next_cursor` must reach the wire so a caller can page on.
#[tokio::test]
async fn list_lines_threads_filter_and_surfaces_page() {
    let payer = uuid::uuid!("22222222-2222-2222-2222-222222222222");
    let router = router_with_stubs(
        Arc::new(ReadStubClient) as Arc<dyn LedgerClientV1>,
        Arc::new(StubPoster) as Arc<dyn InvoicePoster>,
    )
    .layer(axum::Extension(authed_context()));
    // `$filter` carries the payer equality; seller is the caller when omitted.
    // `limit` is the plain pagination cap. `%20` encodes the spaces in the
    // OData expression `payer_tenant_id eq <uuid>`.
    let filter = format!("payer_tenant_id%20eq%20{payer}");
    let response = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!(
                    "/bss-ledger/v1/journal-lines?$filter={filter}&limit=1"
                ))
                .body(Body::empty())
                .expect("build req"),
        )
        .await
        .expect("send");

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("body must be JSON");
    // The items array carries the stub line, with the `$filter` payer echoed.
    assert_eq!(
        value["items"][0]["payer_tenant_id"],
        serde_json::json!(payer.to_string()),
        "the $filter payer_tenant_id threaded into the client call"
    );
    assert_eq!(value["items"][0]["amount_minor"], serde_json::json!(1200));
    // The continuation token rides in `page_info.next_cursor` (canonical Page).
    assert_eq!(
        value["page_info"]["next_cursor"],
        serde_json::json!(NEXT_CURSOR),
        "the page's next_cursor must reach the wire under page_info"
    );
}

#[tokio::test]
async fn list_lines_named_tenant_id_returns_400() {
    let router = router_with_stubs(
        Arc::new(ReadStubClient) as Arc<dyn LedgerClientV1>,
        Arc::new(StubPoster) as Arc<dyn InvoicePoster>,
    )
    .layer(axum::Extension(authed_context()));
    let response = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!(
                    "/bss-ledger/v1/journal-lines?tenant_id={SUBJECT_TENANT}"
                ))
                .body(Body::empty())
                .expect("build req"),
        )
        .await
        .expect("send");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn list_journal_entries_ungranted_filter_tenant_returns_403() {
    let other = uuid::uuid!("dddddddd-eeee-ffff-0000-111111111111");
    let router = base_router().layer(axum::Extension(authed_context()));
    let response = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!(
                    "/bss-ledger/v1/journal-entries?$filter=tenant_id%20eq%20{other}"
                ))
                .body(Body::empty())
                .expect("build req"),
        )
        .await
        .expect("send");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

/// `GET /balances/ar-aging` returns the bucketed AR shape: the stub
/// `list_ar_invoice_balances` returns two past-due AR rows for one payer (≈45 and
/// ≈10 days past due), and the handler folds them through
/// [`crate::domain::invoice::aging`] into `(payer, currency, bucket, amount)`
/// grains — a `31-60` and a `1-30` bucket for the payer.
#[tokio::test]
async fn ar_aging_returns_bucketed_shape() {
    let router = router_with_stubs(
        Arc::new(ReadStubClient) as Arc<dyn LedgerClientV1>,
        Arc::new(StubPoster) as Arc<dyn InvoicePoster>,
    )
    .layer(axum::Extension(authed_context()));
    let response = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!(
                    "/bss-ledger/v1/balances/ar-aging?tenant_id={SUBJECT_TENANT}&payer={AGING_PAYER}"
                ))
                .body(Body::empty())
                .expect("build req"),
        )
        .await
        .expect("send");

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("body must be JSON");
    let buckets = value["buckets"]
        .as_array()
        .expect("buckets must be an array");
    // Two grains: the ≈45-day row → 31-60 (5000), the ≈10-day row → 1-30 (3000).
    let find = |bucket: &str| {
        buckets.iter().find(|b| {
            b["bucket"] == serde_json::json!(bucket)
                && b["payer_tenant_id"] == serde_json::json!(AGING_PAYER.to_string())
        })
    };
    let b_31_60 = find("31-60").expect("a 31-60 bucket for the payer");
    assert_eq!(
        b_31_60["amount_minor"],
        serde_json::json!(5000),
        "the ≈45-day-past-due invoice ages into 31-60"
    );
    let b_1_30 = find("1-30").expect("a 1-30 bucket for the payer");
    assert_eq!(
        b_1_30["amount_minor"],
        serde_json::json!(3000),
        "the ≈10-day-past-due invoice ages into 1-30"
    );
    assert_eq!(b_1_30["currency"], serde_json::json!("USD"));
}

/// `GET /journal-entries/{entryId}` carries the audit who/when/source dims
/// (AC #8): the response DTO must surface `posted_by_actor_id`, `posted_at_utc`,
/// `origin`, `source_doc_type`, `source_business_id`, and `correlation_id` so a
/// caller can audit who posted the entry, when, and from which source document.
#[tokio::test]
async fn get_entry_carries_audit_who_when_source() {
    let entry = uuid::uuid!("eeeeeeee-eeee-eeee-eeee-eeeeeeeeeeee");
    let router = router_with_stubs(
        Arc::new(ReadStubClient) as Arc<dyn LedgerClientV1>,
        Arc::new(StubPoster) as Arc<dyn InvoicePoster>,
    )
    .layer(axum::Extension(authed_context()));
    let response = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/bss-ledger/v1/journal-entries/{entry}"))
                .body(Body::empty())
                .expect("build req"),
        )
        .await
        .expect("send");

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("body must be JSON");
    // WHO posted it.
    assert_eq!(
        value["posted_by_actor_id"],
        serde_json::json!(AUDIT_ACTOR.to_string()),
        "the actor who posted the entry must be carried"
    );
    assert_eq!(
        value["correlation_id"],
        serde_json::json!(AUDIT_CORRELATION.to_string())
    );
    assert_eq!(value["origin"], serde_json::json!(AUDIT_ORIGIN));
    // WHEN — `posted_at_utc` must be present (a parseable RFC-3339 timestamp).
    assert!(
        value["posted_at_utc"].is_string(),
        "posted_at_utc must be carried, got {value}"
    );
    // FROM WHICH SOURCE document.
    assert_eq!(value["source_doc_type"], serde_json::json!("INVOICE_POST"));
    assert_eq!(
        value["source_business_id"],
        serde_json::json!(AUDIT_BUSINESS_ID)
    );
}

/// Write stub that ACCEPTS reversal + correction posts (returns a fresh ref) — so
/// the reverse / mapping-correction handler happy paths reach a posting response.
/// (`ReadStubClient::get_entry` returns an INVOICE_POST entry, so `build_reversal`
/// does not trip `CANNOT_REVERSE_REVERSAL`.)
struct WorkingPoster;

#[async_trait]
impl InvoicePoster for WorkingPoster {
    async fn post_invoice(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _scope: &AccessScope,
        _inv: &PostedInvoice,
        _payer_open: bool,
    ) -> Result<PostingRef, DomainError> {
        unimplemented!("reverse / correction post via post_reversal / post_correction")
    }

    async fn post_reversal(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _scope: &AccessScope,
        _reversal: PostEntry,
        _reason: Option<String>,
    ) -> Result<PostingRef, DomainError> {
        Ok(PostingRef {
            entry_id: STUB_ENTRY,
            created_seq: 1,
            replayed: false,
        })
    }

    async fn post_correction(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _scope: &AccessScope,
        _correction: PostEntry,
    ) -> Result<PostingRef, DomainError> {
        Ok(PostingRef {
            entry_id: STUB_ENTRY,
            created_seq: 2,
            replayed: false,
        })
    }
}

/// The reverse handler happy path: read the original (INVOICE_POST) → gate
/// `(entry, reverse)` → build + post the reversal → a success posting response
/// (no dual-control wired ⇒ inline).
#[tokio::test]
async fn reverse_entry_happy_path_posts_reversal() {
    let router = router_with_stubs(Arc::new(ReadStubClient), Arc::new(WorkingPoster))
        .layer(axum::Extension(authed_context()));
    let entry = Uuid::now_v7();
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/bss-ledger/v1/journal-entries/{entry}/reversals"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "reason": "e2e reverse" }).to_string(),
                ))
                .expect("build req"),
        )
        .await
        .expect("router call");
    assert!(
        response.status().is_success(),
        "reverse happy path must succeed, got {}",
        response.status()
    );
}

/// The mapping-correction handler happy path: read the original → gate → reverse
/// the mis-mapped original → re-post the corrected lines → a success response.
#[tokio::test]
async fn correct_mapping_happy_path_posts_correction() {
    let router = router_with_stubs(Arc::new(ReadStubClient), Arc::new(WorkingPoster))
        .layer(axum::Extension(authed_context()));
    let entry = Uuid::now_v7();
    let body = serde_json::json!({
        "reason": "remap to revenue",
        "corrected_items": [
            {
                "amount_minor_ex_tax": 1000,
                "currency": "USD",
                "revenue_stream": "subscription",
                "catalog_class": "REVENUE",
                "gl_code": "4000"
            }
        ]
    });
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/bss-ledger/v1/journal-entries/{entry}/mapping-corrections"
                ))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .expect("build req"),
        )
        .await
        .expect("router call");
    assert!(
        response.status().is_success(),
        "mapping-correction happy path must succeed, got {}",
        response.status()
    );
}
