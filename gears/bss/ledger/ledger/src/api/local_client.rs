//! `LedgerLocalClient` — the in-process implementation of
//! [`LedgerClientV1`] published in `ClientHub`. It maps the SDK
//! request DTOs (`PostEntry`/`PostLine`) onto the gear-internal
//! `NewEntry`/`NewLine`, resolving each line's `currency_scale` through the
//! [`CurrencyScaleResolver`], then delegates to [`PostingService`].

use std::str::FromStr;

use bss_ledger_sdk::api::LedgerClientV1;
use bss_ledger_sdk::{
    AccountClass, AccountInfo, AllocateOutcome, AllocatePayment, AllocationApplied,
    AllocationQueued, AllocationSplit, AllocationView, ArInvoiceBalanceView, BalanceView,
    ChangeRecognitionSchedule, CloseOutcome, CreditApplication, CreditApplicationApplied,
    CreditDebitView, DisputeOutcome, DisputeQueued, DisputeRecorded, EntryView, LineView,
    MappingStatus, ODataQuery, Page, PostEntry, PostingRef, ProvisionOutcome, ProvisionRequest,
    RecognitionRunOutcome, RecognitionScheduleList, RecognitionScheduleSegmentView,
    RecognitionScheduleSummaryView, RecognitionScheduleView, RecordDisputePhase, ReturnPayment,
    RevenueDisaggregation, RevenueDisaggregationEntry, RevenueDisaggregationQuery,
    ScheduleChangeRef, SettlePayment, Side, SourceDocType, TriggerRecognitionRun, UnallocatedView,
};
use sea_orm::{ColumnTrait, Condition, EntityTrait};
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit_db::secure::SecureEntityExt;
use toolkit_db::{DBProvider, DbError};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::error::authz_error_to_canonical;
use crate::domain::error::DomainError;
use crate::domain::model::{EntryRecord, LineRecord, NewEntry, NewLine};
use crate::infra::currency_scale::CurrencyScaleResolver;
use crate::infra::period_close::PeriodCloseService;
use crate::infra::posting::service::PostingService;
use crate::infra::provisioning::service::ProvisioningService;
use crate::infra::storage::entity::{
    account_balance, ar_invoice_balance, journal_line, payment_allocation, recognition_schedule,
    recognition_segment,
};
use crate::infra::storage::repo::JournalRepo;
use crate::infra::storage::repo::journal_repo::OdataPageError;
use time::OffsetDateTime;

/// Origin literal stamped on posts made through the in-process client until a
/// caller-supplied origin is threaded from the security context.
const ORIGIN_SYSTEM: &str = "SYSTEM";

/// In-process `LedgerClientV1` over the posting engine.
pub struct LedgerLocalClient {
    posting: PostingService,
    db: DBProvider<DbError>,
    resolver: CurrencyScaleResolver,
    provisioning: ProvisioningService,
    period_close: PeriodCloseService,
    // Payment money-in (settle a receipt into the unallocated pool) and money-out
    // (allocate the pool to open AR oldest-first). Each holds its own posting
    // engine + publisher + metrics clones.
    settle_service: crate::infra::payment::settle::SettlementService,
    // Settlement return (money-in reversal): claws a settled receipt back out of
    // the pool, decrementing `settled_minor`. Same deps as `settle_service`.
    settlement_return_service: crate::infra::payment::settlement_return::SettlementReturnService,
    // Chargeback dispute (open / win / lose): records a dispute phase, seeding /
    // advancing the `ledger_dispute` state. Same deps as `settle_service`.
    chargeback_service: crate::infra::payment::chargeback::ChargebackService,
    allocate_service: crate::infra::payment::allocate::AllocationService,
    // Reusable-credit wallet (architecture §5.2): grant parks pool cash into the
    // wallet, apply spends it against open AR. Same deps as `allocate_service`.
    credit_service: crate::infra::payment::credit::CreditApplicationService,
    // ASC 606 recognition run (architecture §5, the S6 release): orchestrates one
    // `RecognitionRunner` pass with the `recognition_run` row lifecycle. Same
    // db.clone() + publisher clone as the payment services.
    recognition_run_service: crate::infra::recognition::run_service::RecognitionRunService,
    // ASC 606 schedule change / cancel (Group H, design §3.6): marks an ACTIVE
    // schedule terminal (CANCELLED/REPLACED) and mints a prospective successor on
    // a replace, in one serializable txn, emitting `schedule.changed` in-txn. Same
    // db.clone() + publisher clone (a change posts no journal entry).
    recognition_change_service: crate::infra::recognition::change_service::RecognitionChangeService,
    // Held so the live producers stay alive for the client's lifetime; the
    // posting engine clones its own `Arc` into `PostingService`.
    #[allow(dead_code)]
    publisher: std::sync::Arc<crate::infra::events::publisher::LedgerEventPublisher>,
    // Platform PEP: derives the per-call `AccessScope` (reads) and gates
    // writes/admin actions before the repository is touched.
    enforcer: std::sync::Arc<authz_resolver_sdk::PolicyEnforcer>,
    // Provisioning seller-type gate: only a tenant whose type owns a billing
    // ledger may be provisioned (§4.12); reads AM + the Types Registry.
    seller_guard: std::sync::Arc<crate::infra::seller_guard::SellerGuard>,
    // OTel metrics handle. Stored now (the invoice-post domain emits
    // `invoice_post` / `invoice_post_duration` from the post path); held so the
    // process-global instruments are reachable from the client.
    #[allow(dead_code)]
    metrics: std::sync::Arc<dyn crate::domain::ports::metrics::LedgerMetricsPort>,
}

impl LedgerLocalClient {
    /// Build the client over one database provider, the event publisher
    /// (built once at `init()`; a no-op publisher when the broker is absent),
    /// and the platform PEP (used by the P7 gates to scope reads and gate
    /// writes/admin actions).
    #[must_use]
    pub(crate) fn new(
        db: DBProvider<DbError>,
        publisher: std::sync::Arc<crate::infra::events::publisher::LedgerEventPublisher>,
        enforcer: std::sync::Arc<authz_resolver_sdk::PolicyEnforcer>,
        seller_guard: std::sync::Arc<crate::infra::seller_guard::SellerGuard>,
        metrics: std::sync::Arc<dyn crate::domain::ports::metrics::LedgerMetricsPort>,
        // Slice 5: FX tunables (provider order + staleness) for the S2 settle lock.
        fx_config: crate::config::FxConfig,
        // Slice 3: payments tunables (the per-allocation touched-invoice cap).
        payments_config: crate::config::PaymentsConfig,
        // Slice 7 Phase 3: the pre-close control-feed gate inputs (manifest completeness +
        // bill-run-finished, flag-gated). `CloseControlFeeds::inert()` disables both gates.
        close_control: crate::infra::period_close::CloseControlFeeds,
    ) -> Self {
        let posting = PostingService::new(db.clone(), std::sync::Arc::clone(&publisher));
        let resolver =
            CurrencyScaleResolver::new(crate::infra::storage::repo::ReferenceRepo::new(db.clone()));
        let provisioning = ProvisioningService::new(db.clone());
        let period_close = PeriodCloseService::new(
            db.clone(),
            std::sync::Arc::clone(&publisher),
            std::sync::Arc::new(
                crate::infra::audit::secured_audit_sink::NoopSecuredAuditSink::new(),
            ),
        )
        // Slice 7 Phase 3: the gated close consults the manifest / bill-run feeds (flag-gated).
        .with_control_feeds(close_control);
        let settle_service = crate::infra::payment::settle::SettlementService::new(
            db.clone(),
            std::sync::Arc::clone(&publisher),
            std::sync::Arc::clone(&metrics),
        )
        // Slice 5 S2: the settle FX lock (inert for a single-currency tenant —
        // RateLocker short-circuits when the tenant has no functional currency).
        .with_fx(crate::infra::fx::rate_locker::RateLocker::new(
            crate::infra::fx::rate_source::RateSource::new(
                crate::infra::storage::repo::FxRepo::new(db.clone()),
                fx_config,
            )
            .with_metrics(std::sync::Arc::clone(&metrics)),
            crate::infra::storage::repo::FxRepo::new(db.clone()),
        ));
        // Slice 7 Phase 2: the exception router (additive close-blocking routing) is
        // shared across the in-process client's stub-bearing services (chargeback +
        // settlement-return). Built over the same db (stateless handle).
        let exception_router = crate::infra::exception::ExceptionRouter::shared(db.clone());
        let settlement_return_service =
            crate::infra::payment::settlement_return::SettlementReturnService::new(
                db.clone(),
                std::sync::Arc::clone(&publisher),
                std::sync::Arc::clone(&metrics),
            )
            .with_exceptions(std::sync::Arc::clone(&exception_router));
        let chargeback_service = crate::infra::payment::chargeback::ChargebackService::new(
            db.clone(),
            std::sync::Arc::clone(&publisher),
            std::sync::Arc::clone(&metrics),
        )
        .with_exceptions(std::sync::Arc::clone(&exception_router));
        let allocate_service = crate::infra::payment::allocate::AllocationService::new(
            db.clone(),
            std::sync::Arc::clone(&publisher),
            std::sync::Arc::clone(&metrics),
        )
        .with_max_invoices_per_allocation(payments_config.max_invoices_per_allocation);
        let credit_service = crate::infra::payment::credit::CreditApplicationService::new(
            db.clone(),
            std::sync::Arc::clone(&publisher),
            std::sync::Arc::clone(&metrics),
        );
        // Recognition run-service: same db.clone() + publisher clone as the
        // payment services (the runner's posting engine clones the publisher
        // Arc), plus the metrics handle for the §9 recognition metrics (run
        // duration, recognized-minor, over-recognition / double-credit, queue
        // depth).
        let recognition_run_service =
            crate::infra::recognition::run_service::RecognitionRunService::new(
                db.clone(),
                std::sync::Arc::clone(&publisher),
                std::sync::Arc::clone(&metrics),
            );
        // Schedule change/cancel (Group H): a change posts no journal entry, so the
        // service needs only the db + publisher (the in-txn `schedule.changed`
        // emit) — no metrics handle.
        let recognition_change_service =
            crate::infra::recognition::change_service::RecognitionChangeService::new(
                db.clone(),
                std::sync::Arc::clone(&publisher),
            );
        Self {
            posting,
            db,
            resolver,
            provisioning,
            period_close,
            settle_service,
            settlement_return_service,
            chargeback_service,
            allocate_service,
            credit_service,
            recognition_run_service,
            recognition_change_service,
            publisher,
            enforcer,
            seller_guard,
            metrics,
        }
    }

    /// Derive the caller's compiled `In` read scope on the `entry` data plane.
    /// `require_constraints = true` so an unconstrained allow fail-closes
    /// rather than leaking every tenant; the returned scope is the SQL filter
    /// `SecureORM` binds to `tenant_id` (SQL-level BOLA). Shared by every ledger
    /// read method.
    async fn read_entry_scope(
        &self,
        ctx: &SecurityContext,
    ) -> Result<toolkit_security::AccessScope, CanonicalError> {
        crate::authz::access_scope(
            &self.enforcer,
            ctx,
            &crate::authz::resource_types::ENTRY,
            crate::authz::actions::READ,
            None,
            None,
            true,
        )
        .await
        .map_err(authz_error_to_canonical)
    }

    /// Derive the caller's compiled `In` read scope on the `recognition` resource
    /// (runs / schedules / disaggregation). Mirrors [`Self::read_entry_scope`] but on
    /// the recognition revenue plane, so a revenue-accountant grant reads recognition
    /// without the `entry` data plane.
    async fn read_recognition_scope(
        &self,
        ctx: &SecurityContext,
    ) -> Result<toolkit_security::AccessScope, CanonicalError> {
        crate::authz::access_scope(
            &self.enforcer,
            ctx,
            &crate::authz::resource_types::RECOGNITION,
            crate::authz::actions::READ,
            None,
            None,
            true,
        )
        .await
        .map_err(authz_error_to_canonical)
    }
}

#[async_trait::async_trait]
impl LedgerClientV1 for LedgerLocalClient {
    async fn post_balanced_entry(
        &self,
        ctx: &SecurityContext,
        entry: PostEntry,
    ) -> Result<PostingRef, CanonicalError> {
        // Write gate + scope: authorize (entry, post) against the entry's
        // target tenant; the returned scope threads into the scoped post.
        let scope = crate::authz::access_scope(
            &self.enforcer,
            ctx,
            &crate::authz::resource_types::ENTRY,
            crate::authz::actions::POST,
            Some(entry.tenant_id),
            None,
            true,
        )
        .await
        .map_err(authz_error_to_canonical)?;
        let posted_at_utc = OffsetDateTime::now_utc();

        let new_entry = NewEntry {
            entry_id: entry.entry_id,
            tenant_id: entry.tenant_id,
            // v1: one legal entity per tenant — derived server-side, never
            // taken from the caller (column retained for future multi-LE).
            legal_entity_id: entry.tenant_id,
            period_id: entry.period_id,
            entry_currency: entry.entry_currency,
            source_doc_type: entry.source_doc_type,
            source_business_id: entry.source_business_id,
            reverses_entry_id: entry.reverses_entry_id,
            reverses_period_id: entry.reverses_period_id,
            posted_at_utc,
            effective_at: entry.effective_at,
            origin: ORIGIN_SYSTEM.to_owned(),
            // Audit actor is the authenticated subject, stamped server-side —
            // NEVER the caller-supplied `entry.posted_by_actor_id`. The REST DTO
            // already lowers with `ctx.subject_id()`, but an in-process ClientHub
            // caller could otherwise attribute a post to an arbitrary subject;
            // clamp it to the security context here so no path can spoof it.
            posted_by_actor_id: ctx.subject_id(),
            correlation_id: entry.correlation_id,
            rounding_evidence: serde_json::Value::Null,
            // Slice 5: this direct-post path is same-currency in v1 (no FX lock).
            rate_snapshot_ref: None,
        };

        let mut new_lines: Vec<NewLine> = Vec::with_capacity(entry.lines.len());
        for line in entry.lines {
            // Functional FX columns must be a consistent pair on this direct
            // in-process post path: there is no RateLocker here to derive one
            // from the other (`rate_snapshot_ref` is `None`), so a `Some` amount
            // with a `None` currency (or vice versa) would persist a
            // half-populated, unauditable dual column the projector then reads
            // inconsistently. Reject the mismatch as a 400.
            if line.functional_amount_minor.is_some() != line.functional_currency.is_some() {
                return Err(DomainError::InvalidRequest(
                    "functional_amount_minor and functional_currency must both be set or both be \
                     null on a direct post"
                        .to_owned(),
                )
                .into());
            }
            let scale = self
                .resolver
                .resolve(&scope, entry.tenant_id, &line.currency)
                .await
                .map_err(|e| {
                    CanonicalError::internal(format!("currency scale resolve: {e}")).create()
                })?;
            new_lines.push(NewLine {
                line_id: line.line_id,
                payer_tenant_id: line.payer_tenant_id,
                seller_tenant_id: line.seller_tenant_id,
                resource_tenant_id: line.resource_tenant_id,
                account_id: line.account_id,
                account_class: line.account_class,
                gl_code: line.gl_code,
                side: line.side,
                amount_minor: line.amount_minor,
                currency: line.currency,
                currency_scale: scale,
                invoice_id: line.invoice_id,
                due_date: line.due_date,
                revenue_stream: line.revenue_stream,
                mapping_status: line.mapping_status,
                functional_amount_minor: line.functional_amount_minor,
                functional_currency: line.functional_currency,
                tax_jurisdiction: line.tax_jurisdiction,
                tax_filing_period: line.tax_filing_period,
                tax_rate_ref: line.tax_rate_ref,
                legal_entity_id: None,
                invoice_item_ref: line.invoice_item_ref,
                sku_or_plan_ref: line.sku_or_plan_ref,
                price_id: line.price_id,
                pricing_snapshot_ref: line.pricing_snapshot_ref,
                po_allocation_group: line.po_allocation_group,
                credit_grant_event_type: line.credit_grant_event_type,
                ar_status: line.ar_status,
            });
        }

        self.posting
            .post(ctx, &scope, new_entry, new_lines, None)
            .await
            .map_err(CanonicalError::from)
    }

    async fn read_account_balance(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        account_id: Uuid,
    ) -> Result<Option<i64>, CanonicalError> {
        // Read scope: the PDP returns the caller's compiled `In` scope, which
        // SecureORM binds to `tenant_id` (SQL-level BOLA). `require_constraints`
        // is true so an unconstrained allow fail-closes rather than leaking.
        let scope = crate::authz::access_scope(
            &self.enforcer,
            ctx,
            &crate::authz::resource_types::ENTRY,
            crate::authz::actions::READ,
            None,
            None,
            true,
        )
        .await
        .map_err(authz_error_to_canonical)?;
        let conn = self
            .db
            .conn()
            .map_err(|e| CanonicalError::internal(format!("conn: {e}")).create())?;
        let row = account_balance::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(
                // Single-currency account (v1: one account per currency), so the
                // (tenant, account) grain identifies exactly one balance row.
                Condition::all()
                    .add(account_balance::Column::TenantId.eq(tenant_id))
                    .add(account_balance::Column::AccountId.eq(account_id)),
            )
            .one(&conn)
            .await
            .map_err(|e| CanonicalError::internal(format!("read account_balance: {e}")).create())?;
        Ok(row.map(|r| r.balance_minor))
    }

    async fn list_accounts(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<AccountInfo>, CanonicalError> {
        // Read scope: list the chart of accounts on the `ledger` config plane.
        // The PDP returns the caller's compiled `In` scope, which SecureORM binds
        // to `tenant_id` (SQL-level BOLA). A tenant outside the caller's scope
        // yields no rows — no existence leak, no 403. The user `$filter` ANDs
        // this scope (never replaces it).
        let scope = crate::authz::access_scope(
            &self.enforcer,
            ctx,
            &crate::authz::resource_types::LEDGER,
            crate::authz::actions::READ,
            None,
            None,
            true,
        )
        .await
        .map_err(authz_error_to_canonical)?;
        let repo = crate::infra::storage::repo::ReferenceRepo::new(self.db.clone());
        let page = repo
            .list_accounts(&scope, tenant_id, query)
            .await
            .map_err(map_odata_page_err)?;
        let items = page
            .items
            .into_iter()
            .map(|r| {
                Ok(AccountInfo {
                    account_id: r.account_id,
                    account_class: AccountClass::from_str(&r.account_class).map_err(|e| {
                        CanonicalError::internal(format!("bad account_class: {e}")).create()
                    })?,
                    currency: r.currency,
                    revenue_stream: r.revenue_stream,
                    lifecycle_state: r.lifecycle_state,
                })
            })
            .collect::<Result<Vec<_>, CanonicalError>>()?;
        Ok(Page {
            items,
            page_info: page.page_info,
        })
    }

    async fn get_entry(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        entry_id: Uuid,
    ) -> Result<Option<EntryView>, CanonicalError> {
        // Read scope on the `entry` data plane: the PDP returns the caller's
        // compiled `In` scope, which SecureORM binds to `tenant_id` (SQL-level
        // BOLA). A foreign-owned entry resolves to `None` — no existence leak.
        let scope = self.read_entry_scope(ctx).await?;
        let record = JournalRepo::new(self.db.clone())
            .find_entry_with_lines(&scope, tenant_id, entry_id)
            .await
            .map_err(|e| CanonicalError::internal(format!("get entry: {e}")).create())?;
        record.map(entry_record_to_view).transpose()
    }

    async fn list_lines(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<LineView>, CanonicalError> {
        let scope = self.read_entry_scope(ctx).await?;
        // The canonical `$filter` (payer / account_class / period / invoice_id)
        // + cursor + limit are lowered by `paginate_odata` in the repo, additive
        // over the SecureORM scope (SQL-level BOLA). The legacy
        // `source_business_id` entry-header resolution is gone — a caller filters
        // the line's own `invoice_id` directly.
        let page = JournalRepo::new(self.db.clone())
            .list_lines(&scope, tenant_id, query)
            .await
            .map_err(map_odata_page_err)?;
        let items = page
            .items
            .into_iter()
            .map(line_model_to_view)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Page {
            items,
            page_info: page.page_info,
        })
    }

    async fn list_balances(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<BalanceView>, CanonicalError> {
        let scope = self.read_entry_scope(ctx).await?;
        let page = JournalRepo::new(self.db.clone())
            .list_balances(&scope, tenant_id, query)
            .await
            .map_err(map_odata_page_err)?;
        let items = page
            .items
            .into_iter()
            .map(balance_model_to_view)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Page {
            items,
            page_info: page.page_info,
        })
    }

    async fn list_ar_invoice_balances(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        payer_tenant_id: Option<Uuid>,
    ) -> Result<Vec<ArInvoiceBalanceView>, CanonicalError> {
        let scope = self.read_entry_scope(ctx).await?;
        let rows = JournalRepo::new(self.db.clone())
            .list_ar_invoice_balances(&scope, tenant_id, payer_tenant_id)
            .await
            .map_err(|e| {
                CanonicalError::internal(format!("list ar invoice balances: {e}")).create()
            })?;
        Ok(rows.into_iter().map(ar_invoice_model_to_view).collect())
    }

    async fn provision(
        &self,
        ctx: &SecurityContext,
        req: ProvisionRequest,
    ) -> Result<ProvisionOutcome, CanonicalError> {
        // In-process defence-in-depth: the REST layer already 403s on a denied
        // (ledger, provision); gate again here against the request's tenant.
        crate::authz::access_scope(
            &self.enforcer,
            ctx,
            &crate::authz::resource_types::LEDGER,
            crate::authz::actions::PROVISION,
            Some(req.tenant_id),
            None,
            true,
        )
        .await
        .map_err(authz_error_to_canonical)?;
        // §4.12 predicate: only a tenant whose TYPE owns a billing ledger may be
        // provisioned (a buyer/leaf type is rejected with FailedPrecondition).
        self.seller_guard
            .assert_owns_ledger(ctx, req.tenant_id)
            .await?;
        self.provisioning
            .provision(req)
            .await
            .map_err(CanonicalError::from)
    }

    async fn close_period(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        period_id: String,
    ) -> Result<CloseOutcome, CanonicalError> {
        // Authorize (fiscal_period, close) against the tenant before the
        // OPEN→CLOSED transition.
        crate::authz::access_scope(
            &self.enforcer,
            ctx,
            &crate::authz::resource_types::FISCAL_PERIOD,
            crate::authz::actions::CLOSE,
            Some(tenant_id),
            None,
            true,
        )
        .await
        .map_err(authz_error_to_canonical)?;
        // v1: one legal entity per tenant — the close key's LE is the tenant. The
        // `ctx` threads the initiating Finance actor onto the `period_close` row +
        // the `period.closed` event.
        self.period_close
            .close(ctx, tenant_id, tenant_id, period_id)
            .await
            .map_err(CanonicalError::from)
    }

    async fn settle_payment(
        &self,
        ctx: &SecurityContext,
        req: SettlePayment,
    ) -> Result<PostingRef, CanonicalError> {
        // Write gate + scope: authorize (payment, write) against the settlement's
        // target tenant; the returned scope threads into the scoped post.
        let scope = crate::authz::access_scope(
            &self.enforcer,
            ctx,
            &crate::authz::resource_types::PAYMENT,
            crate::authz::actions::WRITE,
            Some(req.tenant_id),
            None,
            true,
        )
        .await
        .map_err(authz_error_to_canonical)?;
        // `scale` is NOT threaded onto the domain input — the per-line currency
        // scale resolver (over the provisioned currency config) is authoritative;
        // the caller's `req.scale` is advisory only.
        let input = crate::domain::payment::settlement::SettlementInput {
            tenant_id: req.tenant_id,
            payer_tenant_id: req.payer_tenant_id,
            payment_id: req.payment_id,
            gross_minor: req.gross_minor,
            fee_minor: req.fee_minor,
            currency: req.currency,
            effective_at: req.effective_at,
        };
        self.settle_service
            .settle(ctx, &scope, input)
            .await
            .map_err(CanonicalError::from)
    }

    async fn return_payment(
        &self,
        ctx: &SecurityContext,
        req: ReturnPayment,
    ) -> Result<PostingRef, CanonicalError> {
        // Write gate + scope: authorize (payment, write) against the return's
        // target tenant; the returned scope threads into the scoped post.
        let scope = crate::authz::access_scope(
            &self.enforcer,
            ctx,
            &crate::authz::resource_types::PAYMENT,
            crate::authz::actions::WRITE,
            Some(req.tenant_id),
            None,
            true,
        )
        .await
        .map_err(authz_error_to_canonical)?;
        // `scale` is advisory (see `settle_payment`) — the per-line currency-scale
        // resolver is authoritative; the caller's `req.scale` is not threaded.
        let input = crate::domain::payment::settlement_return::SettlementReturnInput {
            tenant_id: req.tenant_id,
            payer_tenant_id: req.payer_tenant_id,
            payment_id: req.payment_id,
            psp_return_id: req.psp_return_id,
            amount_minor: req.amount_minor,
            currency: req.currency,
            effective_at: req.effective_at,
        };
        self.settlement_return_service
            .return_settlement(ctx, &scope, input)
            .await
            .map_err(CanonicalError::from)
    }

    async fn record_dispute_phase(
        &self,
        ctx: &SecurityContext,
        req: RecordDisputePhase,
    ) -> Result<DisputeOutcome, CanonicalError> {
        // Write gate + scope: authorize (dispute, write) against the dispute's
        // target tenant; the returned scope threads into the scoped post.
        let scope = crate::authz::access_scope(
            &self.enforcer,
            ctx,
            &crate::authz::resource_types::DISPUTE,
            crate::authz::actions::WRITE,
            Some(req.tenant_id),
            None,
            true,
        )
        .await
        .map_err(authz_error_to_canonical)?;
        // Parse the wire phase / funds-fact literals at the boundary (a bad
        // literal is `InvalidArgument` ⇒ 400, not a deep post-path fault).
        // `scale` is advisory (see `settle_payment`) — not threaded.
        let phase = crate::domain::payment::chargeback::DisputePhase::parse(&req.phase)
            .ok_or_else(|| {
                CanonicalError::from(crate::domain::error::DomainError::InvalidRequest(format!(
                    "unknown dispute phase {:?} (expected opened/won/lost/partial)",
                    req.phase
                )))
            })?;
        let funds_at_open = crate::domain::payment::chargeback::FundsAtOpen::parse(
            &req.funds_at_open,
        )
        .ok_or_else(|| {
            CanonicalError::from(crate::domain::error::DomainError::InvalidRequest(format!(
                "unknown funds_at_open {:?} (expected withheld/not_moved)",
                req.funds_at_open
            )))
        })?;
        let request = crate::infra::payment::chargeback::ChargebackRequest {
            tenant_id: req.tenant_id,
            payer_tenant_id: req.payer_tenant_id,
            payment_id: req.payment_id,
            dispute_id: req.dispute_id,
            invoice_id: req.invoice_id,
            cycle: req.cycle,
            phase,
            funds_at_open,
            disputed_amount_minor: req.disputed_amount_minor,
            currency: req.currency,
            effective_at: req.effective_at,
        };
        // The service returns either an inline post (`Recorded`) or a durable
        // enqueue (`Queued`, §4.7 — an out-of-order `won`/`lost`). Map each onto
        // the SDK outcome; the REST handler renders 201/200-vs-202 from the arm.
        match self
            .chargeback_service
            .record_phase(ctx, &scope, request)
            .await
            .map_err(CanonicalError::from)?
        {
            crate::infra::payment::chargeback::ChargebackOutcome::Recorded(posting) => {
                Ok(DisputeOutcome::Recorded(DisputeRecorded { posting }))
            }
            crate::infra::payment::chargeback::ChargebackOutcome::Queued(queued) => {
                Ok(DisputeOutcome::Queued(DisputeQueued {
                    flow: queued.flow,
                    business_id: queued.business_id,
                    queued_at: queued.queued_at,
                }))
            }
        }
    }

    async fn allocate_payment(
        &self,
        ctx: &SecurityContext,
        req: AllocatePayment,
    ) -> Result<AllocateOutcome, CanonicalError> {
        // Write gate + scope: authorize (payment, write) against the allocation's
        // target tenant.
        let scope = crate::authz::access_scope(
            &self.enforcer,
            ctx,
            &crate::authz::resource_types::PAYMENT,
            crate::authz::actions::WRITE,
            Some(req.tenant_id),
            None,
            true,
        )
        .await
        .map_err(authz_error_to_canonical)?;
        // `scale` is advisory (see `settle_payment`) — not threaded onto the
        // request; the resolver is authoritative.
        let currency = req.currency.clone();
        // Mode B (§4.4 F-5): a caller-supplied split bypasses the precedence
        // decision and is validated against the open candidates by the service.
        let caller_splits = req.splits.map(|splits| {
            splits
                .into_iter()
                .map(|s| crate::domain::payment::precedence::Allocated {
                    invoice_id: s.invoice_id,
                    amount_minor: s.amount_minor,
                })
                .collect()
        });
        let request = crate::infra::payment::allocate::AllocateRequest {
            tenant_id: req.tenant_id,
            payer_tenant_id: req.payer_tenant_id,
            payment_id: req.payment_id,
            allocation_id: req.allocation_id,
            lump_minor: req.lump_minor,
            currency: req.currency,
            hint_invoice_id: req.hint_invoice_id,
            caller_splits,
        };
        // The service returns either an inline post (`Applied`, the payment was
        // settled) or a durable enqueue (`Queued`, §4.7 — the payment was not yet
        // settled). Map each onto the SDK outcome; the REST handler renders
        // 201-vs-202 from the arm.
        match self
            .allocate_service
            .allocate(ctx, &scope, request)
            .await
            .map_err(CanonicalError::from)?
        {
            crate::infra::payment::allocate::AllocationOutcome::Applied(applied) => {
                // Positive-amount splits only; the per-invoice currency is the
                // request currency and `allocated_at_utc` is the apply instant
                // (the sidecar stamps the same `now` on the persisted rows).
                // `policy_ref` is the ref the service stamped — a precedence policy
                // id for the decided path, or `caller-split.v1` for a Mode B split.
                let policy_ref = applied.policy_ref;
                let allocations = applied
                    .splits
                    .into_iter()
                    .map(|s| AllocationView {
                        invoice_id: s.invoice_id,
                        amount_minor: s.amount_minor,
                        currency: currency.clone(),
                        allocated_at_utc: OffsetDateTime::now_utc(),
                        precedence_policy_ref: policy_ref.clone(),
                    })
                    .collect();
                Ok(AllocateOutcome::Applied(AllocationApplied {
                    posting: applied.posting,
                    allocations,
                }))
            }
            crate::infra::payment::allocate::AllocationOutcome::Queued(queued) => {
                Ok(AllocateOutcome::Queued(AllocationQueued {
                    flow: queued.flow,
                    business_id: queued.business_id,
                    queued_at: queued.queued_at,
                }))
            }
        }
    }

    async fn list_payment_allocations(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        payment_id: String,
    ) -> Result<Vec<AllocationView>, CanonicalError> {
        // Read scope on the `payment` data plane: the PDP returns the caller's
        // compiled `In` scope, which SecureORM binds to `tenant_id` (SQL-level
        // BOLA). A foreign-owned payment yields no rows — no existence leak.
        let scope = crate::authz::access_scope(
            &self.enforcer,
            ctx,
            &crate::authz::resource_types::PAYMENT,
            crate::authz::actions::READ,
            None,
            None,
            true,
        )
        .await
        .map_err(authz_error_to_canonical)?;
        let rows = crate::infra::storage::repo::PaymentRepo::new(self.db.clone())
            .list_payment_allocations(&scope, tenant_id, &payment_id)
            .await
            .map_err(|e| {
                CanonicalError::internal(format!("list payment allocations: {e}")).create()
            })?;
        Ok(rows.into_iter().map(payment_allocation_to_view).collect())
    }

    async fn read_unallocated(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        payer_tenant_id: Uuid,
        currency: String,
    ) -> Result<UnallocatedView, CanonicalError> {
        let scope = crate::authz::access_scope(
            &self.enforcer,
            ctx,
            &crate::authz::resource_types::PAYMENT,
            crate::authz::actions::READ,
            None,
            None,
            true,
        )
        .await
        .map_err(authz_error_to_canonical)?;
        let balance_minor = crate::infra::storage::repo::PaymentRepo::new(self.db.clone())
            .read_unallocated(&scope, tenant_id, payer_tenant_id, &currency)
            .await
            .map_err(|e| CanonicalError::internal(format!("read unallocated: {e}")).create())?;
        Ok(UnallocatedView {
            payer_tenant_id,
            currency,
            balance_minor,
        })
    }

    async fn post_credit_application(
        &self,
        ctx: &SecurityContext,
        req: CreditApplication,
    ) -> Result<CreditApplicationApplied, CanonicalError> {
        // Write gate + scope: authorize (credit_application, write) against the
        // operation's target tenant (read from whichever arm the enum carries).
        let scope = crate::authz::access_scope(
            &self.enforcer,
            ctx,
            &crate::authz::resource_types::CREDIT_APPLICATION,
            crate::authz::actions::WRITE,
            Some(req.tenant_id()),
            None,
            true,
        )
        .await
        .map_err(authz_error_to_canonical)?;
        // `scale` is advisory (see `allocate_payment`) — not threaded onto the
        // request; the per-line currency-scale resolver is authoritative.
        let outcome = match req {
            CreditApplication::Grant(g) => {
                self.credit_service
                    .grant_credit(
                        ctx,
                        &scope,
                        crate::infra::payment::credit::GrantRequest {
                            tenant_id: g.tenant_id,
                            payer_tenant_id: g.payer_tenant_id,
                            credit_application_id: g.credit_application_id,
                            currency: g.currency,
                            amount_minor: g.amount_minor,
                            credit_grant_event_type: g.credit_grant_event_type,
                        },
                    )
                    .await
            }
            CreditApplication::Apply(a) => {
                self.credit_service
                    .apply_credit(
                        ctx,
                        &scope,
                        crate::infra::payment::credit::ApplyRequest {
                            tenant_id: a.tenant_id,
                            payer_tenant_id: a.payer_tenant_id,
                            credit_application_id: a.credit_application_id,
                            currency: a.currency,
                            // The per-invoice receivable shares (CR AR side), in
                            // the caller's order, validated by the service.
                            targets: a
                                .targets
                                .into_iter()
                                .map(|s| crate::domain::payment::precedence::Allocated {
                                    invoice_id: s.invoice_id,
                                    amount_minor: s.amount_minor,
                                })
                                .collect(),
                        },
                    )
                    .await
            }
        }
        .map_err(CanonicalError::from)?;
        // Map the domain outcome to the SDK shape: `debits` → per-sub-grain wallet
        // draw-downs, `targets` → per-invoice shares (reusing `AllocationSplit`).
        // A grant leaves both empty.
        Ok(CreditApplicationApplied {
            posting: outcome.posting,
            debits: outcome
                .debits
                .into_iter()
                .map(|d| CreditDebitView {
                    credit_grant_event_type: d.credit_grant_event_type,
                    amount_minor: d.amount_minor,
                })
                .collect(),
            applications: outcome
                .targets
                .into_iter()
                .map(|t| AllocationSplit {
                    invoice_id: t.invoice_id,
                    amount_minor: t.amount_minor,
                })
                .collect(),
        })
    }

    async fn trigger_recognition_run(
        &self,
        ctx: &SecurityContext,
        req: TriggerRecognitionRun,
    ) -> Result<RecognitionRunOutcome, CanonicalError> {
        // Write gate + scope: a recognition run is a revenue-recognition mutation
        // (posts `DR CL / CR Revenue`), so it authorizes `(recognition, write)`
        // against the run's target tenant — recognition's OWN resource, so a
        // revenue-accountant grant triggers runs without the `entry` post right; the
        // returned scope threads into the runner's scoped posts + `recognition_run` writes.
        let scope = crate::authz::access_scope(
            &self.enforcer,
            ctx,
            &crate::authz::resource_types::RECOGNITION,
            crate::authz::actions::WRITE,
            Some(req.tenant_id),
            None,
            true,
        )
        .await
        .map_err(authz_error_to_canonical)?;
        self.recognition_run_service
            .trigger(ctx, &scope, req.tenant_id, &req.period_id, req.run_id)
            .await
            .map_err(CanonicalError::from)
    }

    async fn list_revenue_disaggregation(
        &self,
        ctx: &SecurityContext,
        query: RevenueDisaggregationQuery,
    ) -> Result<RevenueDisaggregation, CanonicalError> {
        // Read scope on the `recognition` resource (revenue plane): the PDP returns
        // the caller's compiled `In` scope, which SecureORM binds to `tenant_id`
        // (SQL-level BOLA). A tenant outside the caller's scope yields no rows — no
        // existence leak. A revenue-accountant grant reads this without `entry.read`.
        let scope = self.read_recognition_scope(ctx).await?;
        let rows = crate::infra::storage::repo::RecognitionRepo::new(self.db.clone())
            .list_revenue_disaggregation(&scope, query.tenant_id, query.period_id.as_deref())
            .await
            .map_err(|e| {
                CanonicalError::internal(format!("list revenue disaggregation: {e}")).create()
            })?;
        Ok(RevenueDisaggregation {
            entries: rows
                .into_iter()
                .map(|r| RevenueDisaggregationEntry {
                    period_id: r.period_id,
                    revenue_stream: r.revenue_stream,
                    recognized_minor: r.recognized_minor,
                    currency: r.currency,
                })
                .collect(),
        })
    }

    async fn change_recognition_schedule(
        &self,
        ctx: &SecurityContext,
        cmd: ChangeRecognitionSchedule,
    ) -> Result<ScheduleChangeRef, CanonicalError> {
        // Write gate + scope: a schedule change marks/mints recognition-schedule
        // state, so it authorizes `(recognition, write)` against the change's target
        // tenant (mirrors `trigger_recognition_run`); the returned scope threads into
        // the change service's scoped reads/writes. Defence-in-depth: the REST layer
        // already gated, and this gates again before the repository is touched.
        let scope = crate::authz::access_scope(
            &self.enforcer,
            ctx,
            &crate::authz::resource_types::RECOGNITION,
            crate::authz::actions::WRITE,
            Some(cmd.tenant_id),
            None,
            true,
        )
        .await
        .map_err(authz_error_to_canonical)?;
        self.recognition_change_service
            .change(ctx, &scope, cmd)
            .await
            .map_err(CanonicalError::from)
    }

    async fn get_recognition_schedule(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        schedule_id: String,
    ) -> Result<Option<RecognitionScheduleView>, CanonicalError> {
        // Read scope on the `recognition` resource (revenue plane), like the
        // disaggregation read: the PDP returns the caller's compiled `In` scope, which
        // SecureORM binds to `tenant_id` (SQL-level BOLA). A schedule outside the
        // caller's subtree resolves to `None` — no existence leak.
        let scope = self.read_recognition_scope(ctx).await?;
        let repo = crate::infra::storage::repo::RecognitionRepo::new(self.db.clone());
        let Some(schedule) = repo
            .read_schedule(&scope, tenant_id, &schedule_id)
            .await
            .map_err(|e| {
                CanonicalError::internal(format!("read recognition schedule: {e}")).create()
            })?
        else {
            // Absent OR scoped-out — the handler renders a 404 either way.
            return Ok(None);
        };
        let segments = repo
            .list_segments(&scope, tenant_id, &schedule_id)
            .await
            .map_err(|e| {
                CanonicalError::internal(format!("list recognition segments: {e}")).create()
            })?;
        Ok(Some(recognition_schedule_to_view(schedule, segments)))
    }

    async fn list_recognition_schedules(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        invoice_id: Option<String>,
        revenue_stream: Option<String>,
    ) -> Result<RecognitionScheduleList, CanonicalError> {
        // Same `recognition` read scope as the by-id read: SecureORM binds it to
        // `tenant_id`, so schedules outside the caller's subtree are excluded
        // (SQL-level BOLA).
        let scope = self.read_recognition_scope(ctx).await?;
        let repo = crate::infra::storage::repo::RecognitionRepo::new(self.db.clone());
        let (rows, truncated) = repo
            .list_schedules(
                &scope,
                tenant_id,
                invoice_id.as_deref(),
                revenue_stream.as_deref(),
            )
            .await
            .map_err(|e| {
                CanonicalError::internal(format!("list recognition schedules: {e}")).create()
            })?;
        Ok(RecognitionScheduleList {
            schedules: rows
                .into_iter()
                .map(recognition_schedule_to_summary)
                .collect(),
            truncated,
        })
    }
}

/// Map a `recognition_schedule` row + its ordered `recognition_segment` rows into
/// the SDK [`RecognitionScheduleView`] lifecycle view. The segments arrive ordered
/// by `segment_no` from [`RecognitionRepo::list_segments`].
fn recognition_schedule_to_view(
    schedule: recognition_schedule::Model,
    segments: Vec<recognition_segment::Model>,
) -> RecognitionScheduleView {
    RecognitionScheduleView {
        schedule_id: schedule.schedule_id,
        status: schedule.status,
        version: schedule.version,
        revenue_stream: schedule.revenue_stream,
        currency: schedule.currency,
        total_deferred_minor: schedule.total_deferred_minor,
        recognized_minor: schedule.recognized_minor,
        source_invoice_id: schedule.source_invoice_id,
        source_invoice_item_ref: schedule.source_invoice_item_ref,
        po_allocation_group: schedule.po_allocation_group,
        subscription_ref: schedule.subscription_ref,
        policy_ref: schedule.policy_ref,
        segments: segments
            .into_iter()
            .map(|seg| RecognitionScheduleSegmentView {
                segment_no: seg.segment_no,
                period_id: seg.period_id,
                amount_minor: seg.amount_minor,
                status: seg.status,
            })
            .collect(),
    }
}

/// Map a `recognition_schedule` row into the SDK [`RecognitionScheduleSummaryView`]
/// header (no segments) — the row shape of the list/discovery surface.
fn recognition_schedule_to_summary(
    schedule: recognition_schedule::Model,
) -> RecognitionScheduleSummaryView {
    RecognitionScheduleSummaryView {
        schedule_id: schedule.schedule_id,
        status: schedule.status,
        version: schedule.version,
        revenue_stream: schedule.revenue_stream,
        currency: schedule.currency,
        total_deferred_minor: schedule.total_deferred_minor,
        recognized_minor: schedule.recognized_minor,
        source_invoice_id: schedule.source_invoice_id,
        source_invoice_item_ref: schedule.source_invoice_item_ref,
        po_allocation_group: schedule.po_allocation_group,
        subscription_ref: schedule.subscription_ref,
        policy_ref: schedule.policy_ref,
    }
}

/// Stored `currency_scale` (`i16` at the DB boundary) → the SDK's `u8`. The
/// scale is always a small non-negative number (≤ the ISO headroom); a
/// negative or out-of-range stored value (impossible by construction) clamps
/// to `0` rather than panicking.
fn scale_to_u8(scale: i16) -> u8 {
    u8::try_from(scale.max(0)).unwrap_or(0)
}

/// Parse a stored enum literal into its SDK enum, mapping an unknown literal to
/// a canonical `Internal` (a stored value outside the enum is data corruption).
fn parse_enum<T, E>(value: &str, parse: impl Fn(&str) -> Result<T, E>) -> Result<T, CanonicalError>
where
    E: std::fmt::Display,
{
    parse(value).map_err(|e| {
        CanonicalError::internal(format!("bad stored enum literal {value:?}: {e}")).create()
    })
}

/// Map a read-back [`LineRecord`] (from `get_entry`) into an SDK [`LineView`].
fn line_record_to_view(r: LineRecord) -> Result<LineView, CanonicalError> {
    Ok(LineView {
        line_id: r.line_id,
        entry_id: r.entry_id,
        payer_tenant_id: r.payer_tenant_id,
        account_id: r.account_id,
        account_class: parse_enum(&r.account_class, AccountClass::from_str)?,
        gl_code: r.gl_code,
        side: parse_enum(&r.side, Side::from_str)?,
        amount_minor: r.amount_minor,
        currency: r.currency,
        currency_scale: scale_to_u8(r.currency_scale),
        invoice_id: r.invoice_id,
        due_date: r.due_date,
        revenue_stream: r.revenue_stream,
        mapping_status: parse_enum(&r.mapping_status, MappingStatus::from_str)?,
        functional_amount_minor: r.functional_amount_minor,
        functional_currency: r.functional_currency,
        tax_jurisdiction: r.tax_jurisdiction,
        tax_filing_period: r.tax_filing_period,
        ar_status: r.ar_status,
    })
}

/// Map a read-back [`EntryRecord`] (header + lines) into an SDK [`EntryView`].
fn entry_record_to_view(r: EntryRecord) -> Result<EntryView, CanonicalError> {
    let lines = r
        .lines
        .into_iter()
        .map(line_record_to_view)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(EntryView {
        entry_id: r.entry_id,
        tenant_id: r.tenant_id,
        period_id: r.period_id,
        entry_currency: r.entry_currency,
        source_doc_type: parse_enum(&r.source_doc_type, SourceDocType::from_str)?,
        source_business_id: r.source_business_id,
        reverses_entry_id: r.reverses_entry_id,
        reverses_period_id: r.reverses_period_id,
        posted_at_utc: r.posted_at_utc,
        effective_at: r.effective_at,
        posted_by_actor_id: r.posted_by_actor_id,
        origin: r.origin,
        correlation_id: r.correlation_id,
        created_seq: r.created_seq,
        lines,
    })
}

/// Map a `journal_line` row (from `list_lines`) into an SDK [`LineView`]. The
/// stored enum literals were written + validated by this gear on the way in; an
/// unknown one here is data corruption and fails loud (`Internal`), never a
/// silently wrong class/side/status.
fn line_model_to_view(m: journal_line::Model) -> Result<LineView, CanonicalError> {
    Ok(LineView {
        line_id: m.line_id,
        entry_id: m.entry_id,
        payer_tenant_id: m.payer_tenant_id,
        account_id: m.account_id,
        account_class: parse_enum(&m.account_class, AccountClass::from_str)?,
        gl_code: m.gl_code,
        side: parse_enum(&m.side, Side::from_str)?,
        amount_minor: m.amount_minor,
        currency: m.currency,
        currency_scale: scale_to_u8(m.currency_scale),
        invoice_id: m.invoice_id,
        due_date: m.due_date,
        revenue_stream: m.revenue_stream,
        mapping_status: parse_enum(&m.mapping_status, MappingStatus::from_str)?,
        functional_amount_minor: m.functional_amount_minor,
        functional_currency: m.functional_currency,
        tax_jurisdiction: m.tax_jurisdiction,
        tax_filing_period: m.tax_filing_period,
        ar_status: m.ar_status,
    })
}

/// Map an `account_balance` row into an SDK [`BalanceView`].
fn balance_model_to_view(m: account_balance::Model) -> Result<BalanceView, CanonicalError> {
    Ok(BalanceView {
        account_id: m.account_id,
        account_class: parse_enum(&m.account_class, AccountClass::from_str)?,
        currency: m.currency,
        balance_minor: m.balance_minor,
        functional_balance_minor: m.functional_balance_minor,
        functional_currency: m.functional_currency,
    })
}

/// Map an `ar_invoice_balance` row into an SDK [`ArInvoiceBalanceView`].
fn ar_invoice_model_to_view(m: ar_invoice_balance::Model) -> ArInvoiceBalanceView {
    ArInvoiceBalanceView {
        payer_tenant_id: m.payer_tenant_id,
        account_id: m.account_id,
        invoice_id: m.invoice_id,
        currency: m.currency,
        balance_minor: m.balance_minor,
        due_date: m.due_date,
    }
}

/// Map a `payment_allocation` row (from `list_payment_allocations`) into an SDK
/// [`AllocationView`].
fn payment_allocation_to_view(m: payment_allocation::Model) -> AllocationView {
    AllocationView {
        invoice_id: m.invoice_id,
        amount_minor: m.amount_minor,
        currency: m.currency,
        allocated_at_utc: m.allocated_at_utc,
        precedence_policy_ref: m.precedence_policy_ref,
    }
}

/// Project an [`OdataPageError`] from an OData-paginated list read into a
/// [`CanonicalError`]. A malformed `$filter` / `$orderby` / cursor
/// (`Odata`) maps through the platform `From<toolkit_odata::Error>` projection
/// (canonical 400 for parse/cursor failures, 500 for an internal driver fault);
/// a connection / storage fault (`Db`) is an `Internal` (500) whose driver text
/// stays out of the wire `Problem`.
pub(crate) fn map_odata_page_err(err: OdataPageError) -> CanonicalError {
    match err {
        OdataPageError::Odata(e) => CanonicalError::from(e),
        OdataPageError::Db(_) => {
            CanonicalError::internal("list read: database error (driver text redacted)").create()
        }
    }
}

#[cfg(test)]
#[path = "local_client_tests.rs"]
mod tests;
