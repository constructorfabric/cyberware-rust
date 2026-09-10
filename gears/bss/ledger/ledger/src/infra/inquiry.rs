//! `InquiryService` + `AuditPackExporter` — the filtered audit-inquiry / drill
//! reads and the audit-pack CSV export (Slice 6 Phase 4 Group 4A, plan
//! `docs/superpowers/plans/2026-06-23-vhp-1858-phase4-inquiry-metrics.md`).
//!
//! All reads go through the `SecureORM` layer (`.secure().scope_with(scope)`),
//! so the scope the caller passes IS the SQL-level BOLA filter — a row outside
//! it is simply never returned. The scope may be a TARGET-tenant scope handed
//! back by [`crate::infra::authz::cross_tenant::CrossTenantGateway`] (an audit
//! pack can be cross-tenant-gated), or the caller's own HOME scope on the
//! routine path.
//!
//! ## Filter axes (verified against the entities)
//! - `legal_entity_id` and `period_id` live on `journal_entry` (the header) and
//!   are filtered there directly.
//! - `payer_tenant_id` and `account_class` live on `journal_line` only, so they
//!   are resolved by reading the scoped lines and matching their parent entries
//!   (the gear keeps no SQL JOIN helper, so this is a two-step scoped read, not
//!   a single joined query).
//! - NOTE: the schema's `period_id` is a `String` (`YYYYMM`), NOT a `Uuid`. The
//!   Group-4A plan sketched `period_id: Option<Uuid>`; the real column is text,
//!   so [`InquiryFilter::period_id`] is an `Option<String>` to match storage.
//!
//! ## Scale / NFR (§10, ratified targets)
//! The MVP export is SYNCHRONOUS: it walks the scoped rows in one request and
//! builds the CSV in memory. The architecture's async-export path (a job that
//! materializes a pack within ≤ 15 min for very large scopes) is a future
//! extension; for the request-time surface the ratified §10 NFR targets are
//! audit retrieval p95 ≤ 2 s, inquiry p95 ≤ 5 s, and audit-pack export async
//! ≤ 15 min — which the bounded scoped reads here meet.

use sea_orm::ActiveValue::Set;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Instant;
use toolkit_db::secure::{AccessScope, DBRunner, DbTx, SecureEntityExt, SecureInsertExt};
use toolkit_db::{DBProvider, DbError};
use uuid::Uuid;

use crate::domain::instant::format_rfc3339;
use crate::domain::model::RepoError;
use crate::domain::ports::metrics::{LedgerMetricsPort, NoopLedgerMetrics};
use crate::infra::storage::entity::{audit_pack_export, journal_entry, journal_line};
use time::OffsetDateTime;

/// The inquiry filter axes. Every field is optional; an absent field is "any".
/// `legal_entity_id` + `period_id` filter the entry header; `payer_tenant_id` +
/// `account_class` filter the lines (and so narrow which entries match).
///
/// Not a `#[domain_model]`: like the sibling read projections in
/// [`crate::infra::audit::retrieval`] (`AuditEntryRecord`, `FreezeRecord`) this
/// is a plain infra read shape, not a domain value.
#[derive(Clone, Debug, Default)]
pub struct InquiryFilter {
    /// Payer tenant on a line (line-axis predicate).
    pub payer_tenant_id: Option<Uuid>,
    /// Fiscal period on the entry header (`YYYYMM`; a `String`, not a `Uuid`).
    pub period_id: Option<String>,
    /// GL account class on a line (line-axis predicate).
    pub account_class: Option<String>,
    /// Legal entity on the entry header.
    pub legal_entity_id: Option<Uuid>,
}

impl InquiryFilter {
    /// `true` when at least one line-axis predicate (`payer_tenant_id` /
    /// `account_class`) is set — the reader must then narrow by line, not header
    /// alone.
    fn has_line_predicate(&self) -> bool {
        self.payer_tenant_id.is_some() || self.account_class.is_some()
    }
}

/// One entry-header row in a filtered inquiry result. A pure read projection of
/// `journal_entry`; carries no lines (the drill read fetches lines).
#[derive(Clone, Debug)]
pub struct EntryRow {
    pub entry_id: Uuid,
    pub tenant_id: Uuid,
    pub legal_entity_id: Uuid,
    pub period_id: String,
    pub entry_currency: String,
    pub source_doc_type: String,
    pub source_business_id: String,
    pub reverses_entry_id: Option<Uuid>,
    pub posted_at_utc: OffsetDateTime,
    pub posted_by_actor_id: Uuid,
    pub origin: String,
    pub correlation_id: Uuid,
    pub created_seq: i64,
}

impl From<journal_entry::Model> for EntryRow {
    fn from(m: journal_entry::Model) -> Self {
        Self {
            entry_id: m.entry_id,
            tenant_id: m.tenant_id,
            legal_entity_id: m.legal_entity_id,
            period_id: m.period_id,
            entry_currency: m.entry_currency,
            source_doc_type: m.source_doc_type,
            source_business_id: m.source_business_id,
            reverses_entry_id: m.reverses_entry_id,
            posted_at_utc: m.posted_at_utc,
            posted_by_actor_id: m.posted_by_actor_id,
            origin: m.origin,
            correlation_id: m.correlation_id,
            created_seq: m.created_seq,
        }
    }
}

/// One line row in a drill / export. A read projection of `journal_line`
/// carrying the linkage columns an audit pack needs.
#[derive(Clone, Debug)]
pub struct LineRow {
    pub line_id: Uuid,
    pub entry_id: Uuid,
    pub payer_tenant_id: Uuid,
    pub account_id: Uuid,
    pub account_class: String,
    pub gl_code: Option<String>,
    pub side: String,
    pub amount_minor: i64,
    pub currency: String,
    pub invoice_id: Option<String>,
    pub revenue_stream: Option<String>,
    pub legal_entity_id: Option<Uuid>,
}

impl From<journal_line::Model> for LineRow {
    fn from(m: journal_line::Model) -> Self {
        Self {
            line_id: m.line_id,
            entry_id: m.entry_id,
            payer_tenant_id: m.payer_tenant_id,
            account_id: m.account_id,
            account_class: m.account_class,
            gl_code: m.gl_code,
            side: m.side,
            amount_minor: m.amount_minor,
            currency: m.currency,
            invoice_id: m.invoice_id,
            revenue_stream: m.revenue_stream,
            legal_entity_id: m.legal_entity_id,
        }
    }
}

/// A drilled entry: the header row, its lines, and the entries linked to it (a
/// reversal / mapping-correction that `reverses_entry_id`-links to this entry,
/// or the entry this one reverses). Mirrors the document-history linkage shape
/// of [`crate::infra::audit::retrieval::AuditRetrievalReader::document_history`].
#[derive(Clone, Debug)]
pub struct EntryDrill {
    pub entry: EntryRow,
    pub lines: Vec<LineRow>,
    /// Entries linked to `entry` (reversals / mapping-corrections that target
    /// it, plus the entry it reverses when set), ordered by `created_seq`.
    pub linked: Vec<EntryRow>,
}

/// Scoped inquiry reader over one [`DBProvider`]. Stateless.
#[derive(Clone)]
pub struct InquiryService {
    db: DBProvider<DbError>,
}

impl InquiryService {
    /// Build the service over one database provider.
    #[must_use]
    pub fn new(db: DBProvider<DbError>) -> Self {
        Self { db }
    }

    /// The underlying provider.
    #[must_use]
    pub fn db(&self) -> &DBProvider<DbError> {
        &self.db
    }

    /// Filter posted entries under `scope` on the service's own connection (the
    /// routine path). Header-axis predicates (`legal_entity_id` / `period_id`)
    /// filter `journal_entry` directly; the line-axis predicates
    /// (`payer_tenant_id` / `account_class`) read the scoped lines and keep only
    /// the entries those lines belong to. Ordered by `created_seq`. SQL-level
    /// BOLA via the scoped selects.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a storage / scope failure.
    pub async fn filter_entries(
        &self,
        scope: &AccessScope,
        filter: &InquiryFilter,
    ) -> Result<Vec<EntryRow>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        filter_entries_on(&conn, scope, filter).await
    }

    /// Drill into one entry by `(tenant_id, entry_id)` under `scope` on the
    /// service's own connection (the routine path). Returns the entry header,
    /// its lines, and the entries linked to it (a reversal / mapping-correction
    /// that targets it, plus the entry it reverses) — mirroring the
    /// document-history linkage shape. `None` when the entry is absent or
    /// outside `scope` (no existence leak).
    ///
    /// # Errors
    /// [`RepoError::Db`] on a storage / scope failure.
    pub async fn drill(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        entry_id: Uuid,
    ) -> Result<Option<EntryDrill>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        drill_on(&conn, scope, tenant_id, entry_id).await
    }
}

/// Filter posted entries under `scope` against any connection (the service's
/// own connection on the routine path, or a `DbTx` on the cross-tenant path so
/// the read shares the forensic record's transaction). See
/// [`InquiryService::filter_entries`] for the predicate semantics.
///
/// # Errors
/// [`RepoError::Db`] on a storage / scope failure.
async fn filter_entries_on<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    filter: &InquiryFilter,
) -> Result<Vec<EntryRow>, RepoError> {
    // Header-axis predicates on journal_entry.
    let mut header_cond = Condition::all();
    if let Some(le) = filter.legal_entity_id {
        header_cond = header_cond.add(journal_entry::Column::LegalEntityId.eq(le));
    }
    if let Some(period) = filter.period_id.clone() {
        header_cond = header_cond.add(journal_entry::Column::PeriodId.eq(period));
    }

    let headers = journal_entry::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(header_cond)
        .order_by(journal_entry::Column::CreatedSeq, Order::Asc)
        .all(conn)
        .await
        .map_err(|e| RepoError::Db(format!("filter journal_entry: {e}")))?;

    if !filter.has_line_predicate() {
        return Ok(headers.into_iter().map(EntryRow::from).collect());
    }

    // Line-axis predicates: read the scoped lines that match, collect their
    // parent entry ids, then keep only the headers in that set.
    let mut line_cond = Condition::all();
    if let Some(payer) = filter.payer_tenant_id {
        line_cond = line_cond.add(journal_line::Column::PayerTenantId.eq(payer));
    }
    if let Some(class) = filter.account_class.clone() {
        line_cond = line_cond.add(journal_line::Column::AccountClass.eq(class));
    }
    let lines = journal_line::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(line_cond)
        .all(conn)
        .await
        .map_err(|e| RepoError::Db(format!("filter journal_line: {e}")))?;
    let matching_entry_ids: std::collections::HashSet<Uuid> =
        lines.into_iter().map(|l| l.entry_id).collect();

    Ok(headers
        .into_iter()
        .filter(|h| matching_entry_ids.contains(&h.entry_id))
        .map(EntryRow::from)
        .collect())
}

/// Drill into one entry by `(tenant_id, entry_id)` under `scope` against any
/// connection: the entry header, its lines, and the entries linked to it
/// (reversal / mapping-correction that link to it, plus the entry it reverses).
/// Reuses the document-history linkage shape of
/// [`crate::infra::audit::retrieval::AuditRetrievalReader::document_history`]
/// for the linked set.
///
/// Returns `None` when the entry is absent or outside `scope` (no existence
/// leak).
///
/// # Errors
/// [`RepoError::Db`] on a storage / scope failure.
async fn drill_on<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    tenant_id: Uuid,
    entry_id: Uuid,
) -> Result<Option<EntryDrill>, RepoError> {
    let header = journal_entry::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(journal_entry::Column::EntryId.eq(entry_id))
                .add(journal_entry::Column::TenantId.eq(tenant_id)),
        )
        .one(conn)
        .await
        .map_err(|e| RepoError::Db(format!("drill journal_entry: {e}")))?;

    let Some(header) = header else {
        return Ok(None);
    };
    let entry = EntryRow::from(header);

    let line_rows = journal_line::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(journal_line::Column::EntryId.eq(entry_id))
                .add(journal_line::Column::TenantId.eq(tenant_id)),
        )
        .order_by(journal_line::Column::LineId, Order::Asc)
        .all(conn)
        .await
        .map_err(|e| RepoError::Db(format!("drill journal_line: {e}")))?;
    let lines = line_rows.into_iter().map(LineRow::from).collect();

    // Linked: entries that reverse THIS entry (reversal / mapping-correction
    // link back via `reverses_entry_id`), plus the entry THIS one reverses.
    let mut seen: std::collections::HashSet<Uuid> = std::collections::HashSet::new();
    seen.insert(entry.entry_id);
    let mut linked: Vec<EntryRow> = Vec::new();

    let reversing = journal_entry::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(journal_entry::Column::TenantId.eq(tenant_id))
                .add(journal_entry::Column::ReversesEntryId.eq(entry_id)),
        )
        .order_by(journal_entry::Column::CreatedSeq, Order::Asc)
        .all(conn)
        .await
        .map_err(|e| RepoError::Db(format!("drill reversing entries: {e}")))?;
    for m in reversing {
        if seen.insert(m.entry_id) {
            linked.push(EntryRow::from(m));
        }
    }

    if let Some(reverses) = entry.reverses_entry_id {
        let prior = journal_entry::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(journal_entry::Column::TenantId.eq(tenant_id))
                    .add(journal_entry::Column::EntryId.eq(reverses)),
            )
            .one(conn)
            .await
            .map_err(|e| RepoError::Db(format!("drill reversed entry: {e}")))?;
        if let Some(m) = prior
            && seen.insert(m.entry_id)
        {
            linked.push(EntryRow::from(m));
        }
    }

    linked.sort_by_key(|e| e.created_seq);
    Ok(Some(EntryDrill {
        entry,
        lines,
        linked,
    }))
}

/// CSV audit-pack exporter over one [`DBProvider`]. Stateless; reuses
/// [`InquiryService`] for the scoped reads.
///
/// The CSV is built BY HAND (no `csv` crate dependency): a header row plus one
/// row per `(entry, line)`, each field RFC-4180-quoted when it contains a
/// comma, double-quote, or newline (wrap in `"`, double any internal `"`).
///
/// MVP is synchronous (in-memory build over the scoped rows). A very large
/// scope would be served by the architecture's async export job (materialize
/// within ≤ 15 min); that path is a future extension. Request-time NFR targets:
/// audit retrieval p95 ≤ 2 s, inquiry p95 ≤ 5 s.
#[derive(Clone)]
pub struct AuditPackExporter {
    inquiry: InquiryService,
    metrics: Arc<dyn LedgerMetricsPort>,
}

impl AuditPackExporter {
    /// Build the exporter over one database provider.
    #[must_use]
    pub fn new(db: DBProvider<DbError>) -> Self {
        Self {
            inquiry: InquiryService::new(db),
            metrics: Arc::new(NoopLedgerMetrics),
        }
    }

    /// Bind the §9 metrics sink (`ledger_audit_pack_export_duration_seconds` is
    /// recorded per export). Defaults to no-op until wired.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<dyn LedgerMetricsPort>) -> Self {
        self.metrics = metrics;
        self
    }

    /// Export a filtered audit pack as CSV under `scope` on the exporter's own
    /// connection (the routine, own-tenant path). Returns the full CSV document
    /// (header + one row per `(entry, line)`) and the data-row count (excludes
    /// the header). SQL-level BOLA via the scoped reads.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a storage / scope failure.
    pub async fn export_csv(
        &self,
        scope: &AccessScope,
        filter: &InquiryFilter,
    ) -> Result<(String, usize), RepoError> {
        let conn = self
            .inquiry
            .db()
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        // §9: record the export latency (`ledger_audit_pack_export_duration_seconds`).
        let started = Instant::now();
        let out = export_csv_on(&conn, scope, filter).await;
        self.metrics
            .audit_pack_export_duration(started.elapsed().as_secs_f64());
        out
    }

    /// Export a filtered audit pack as CSV under `scope` inside `txn` (the
    /// cross-tenant path): the export reads share the transaction in which
    /// [`crate::infra::authz::cross_tenant::CrossTenantGateway::resolve_read_scope`]
    /// wrote the `cross-tenant-access` forensic record, so the record and the
    /// foreign read commit (or roll back) together.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a storage / scope failure.
    pub async fn export_csv_in_txn(
        &self,
        txn: &DbTx<'_>,
        scope: &AccessScope,
        filter: &InquiryFilter,
    ) -> Result<(String, usize), RepoError> {
        // §9: record the export latency (`ledger_audit_pack_export_duration_seconds`).
        let started = Instant::now();
        let out = export_csv_on(txn, scope, filter).await;
        self.metrics
            .audit_pack_export_duration(started.elapsed().as_secs_f64());
        out
    }

    /// Persist a materialized audit-pack export row inside `txn` (the same
    /// transaction that resolved the read scope, wrote the cross-tenant-access
    /// forensic record, and built the CSV — so the export row and the foreign
    /// read commit or roll back together). `scope` MUST be the home-tenant scope
    /// the row is owned by; `scope_with_model` validates the model's `tenant_id`
    /// against it.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a storage / scope failure.
    pub async fn insert_export_in_txn(
        &self,
        txn: &DbTx<'_>,
        scope: &AccessScope,
        model: &audit_pack_export::Model,
    ) -> Result<(), RepoError> {
        let am = audit_pack_export::ActiveModel {
            export_id: Set(model.export_id),
            tenant_id: Set(model.tenant_id),
            target_tenant_id: Set(model.target_tenant_id),
            status: Set(model.status.clone()),
            reason_code: Set(model.reason_code.clone()),
            actor_ref: Set(model.actor_ref.clone()),
            csv: Set(model.csv.clone()),
            row_count: Set(model.row_count),
            error_detail: Set(model.error_detail.clone()),
            created_at_utc: Set(model.created_at_utc),
            completed_at_utc: Set(model.completed_at_utc),
        };
        audit_pack_export::Entity::insert(am.clone())
            .secure()
            .scope_with_model(scope, &am)
            .map_err(|e| RepoError::Db(format!("insert audit_pack_export scope: {e}")))?
            .exec(txn)
            .await
            .map_err(|e| RepoError::Db(format!("insert audit_pack_export: {e}")))?;
        Ok(())
    }

    /// Read one audit-pack export row for `tenant` (the requester's home tenant)
    /// under `scope`, or `None` when absent / scoped-out (no existence leak).
    ///
    /// # Errors
    /// [`RepoError::Db`] on a storage / scope failure.
    pub async fn find_export(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        export_id: Uuid,
    ) -> Result<Option<audit_pack_export::Model>, RepoError> {
        let conn = self
            .inquiry
            .db()
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        audit_pack_export::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(audit_pack_export::Column::TenantId.eq(tenant))
                    .add(audit_pack_export::Column::ExportId.eq(export_id)),
            )
            .one(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("find audit_pack_export: {e}")))
    }
}

/// Build the CSV audit pack under `scope` against any connection (own
/// connection on the routine path, a `DbTx` on the cross-tenant path). The
/// line-axis filter (if any) narrows WHICH entries match; the pack then carries
/// ALL the lines of each matching entry (the full entry is the audit unit).
///
/// # Errors
/// [`RepoError::Db`] on a storage / scope failure.
async fn export_csv_on<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    filter: &InquiryFilter,
) -> Result<(String, usize), RepoError> {
    let entries = filter_entries_on(conn, scope, filter).await?;

    let mut lines_by_entry: HashMap<Uuid, Vec<LineRow>> = HashMap::new();
    let entry_ids: Vec<Uuid> = entries.iter().map(|e| e.entry_id).collect();
    if !entry_ids.is_empty() {
        let line_rows = journal_line::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(Condition::all().add(journal_line::Column::EntryId.is_in(entry_ids)))
            .order_by(journal_line::Column::EntryId, Order::Asc)
            .order_by(journal_line::Column::LineId, Order::Asc)
            .all(conn)
            .await
            .map_err(|e| RepoError::Db(format!("export journal_line: {e}")))?;
        for m in line_rows {
            lines_by_entry
                .entry(m.entry_id)
                .or_default()
                .push(LineRow::from(m));
        }
    }

    let mut csv = String::new();
    csv.push_str(CSV_HEADER);
    csv.push('\n');
    let mut row_count = 0usize;
    for entry in &entries {
        match lines_by_entry.get(&entry.entry_id) {
            Some(lines) if !lines.is_empty() => {
                for line in lines {
                    push_row(&mut csv, entry, Some(line));
                    row_count += 1;
                }
            }
            // An entry with no lines still emits one row (the header dims), so
            // the pack never silently drops an entry.
            _ => {
                push_row(&mut csv, entry, None);
                row_count += 1;
            }
        }
    }
    Ok((csv, row_count))
}

/// The audit-pack CSV header (column order = the row order in [`push_row`]).
const CSV_HEADER: &str = "entry_id,tenant_id,period_id,legal_entity_id,posted_at_utc,\
source_doc_type,source_business_id,origin,posted_by_actor_id,correlation_id,reverses_entry_id,\
created_seq,line_id,payer_tenant_id,account_id,account_class,gl_code,side,amount_minor,currency,\
invoice_id,revenue_stream";

/// Append one CSV data row (an `(entry, line)` pair; `line = None` emits the
/// entry dims with the line columns blank) to `out`, RFC-4180-quoting each
/// field that needs it and terminating with `\n`.
fn push_row(out: &mut String, entry: &EntryRow, line: Option<&LineRow>) {
    let mut fields: Vec<String> = vec![
        entry.entry_id.to_string(),
        entry.tenant_id.to_string(),
        entry.period_id.clone(),
        entry.legal_entity_id.to_string(),
        format_rfc3339(entry.posted_at_utc),
        entry.source_doc_type.clone(),
        entry.source_business_id.clone(),
        entry.origin.clone(),
        entry.posted_by_actor_id.to_string(),
        entry.correlation_id.to_string(),
        entry
            .reverses_entry_id
            .map(|u| u.to_string())
            .unwrap_or_default(),
        entry.created_seq.to_string(),
    ];
    if let Some(line) = line {
        fields.extend([
            line.line_id.to_string(),
            line.payer_tenant_id.to_string(),
            line.account_id.to_string(),
            line.account_class.clone(),
            line.gl_code.clone().unwrap_or_default(),
            line.side.clone(),
            line.amount_minor.to_string(),
            line.currency.clone(),
            line.invoice_id.clone().unwrap_or_default(),
            line.revenue_stream.clone().unwrap_or_default(),
        ]);
    } else {
        // Ten blank line columns (line_id .. revenue_stream).
        for _ in 0..10 {
            fields.push(String::new());
        }
    }

    let mut first = true;
    for f in &fields {
        if !first {
            out.push(',');
        }
        first = false;
        let _ = write!(out, "{}", csv_escape(f));
    }
    out.push('\n');
}

/// RFC-4180 field quoting **plus a CSV formula-injection guard**.
///
/// Two independent transforms, applied in order:
/// 1. **Formula guard:** a field whose first character is one a spreadsheet
///    treats as a formula lead-in (`=`, `+`, `-`, `@`, or a leading TAB/CR) is
///    prefixed with a single quote (`'`) so Excel / Google Sheets render the
///    cell as literal text instead of evaluating it (e.g. `=cmd|…`,
///    `=HYPERLINK(…)`, `=WEBSERVICE(…)` data exfil). The audit-pack carries
///    caller-supplied free text (`source_business_id`, `gl_code`, `invoice_id`,
///    `revenue_stream`, …); without this guard such a value is executed when an
///    investigator opens the pack in a spreadsheet.
/// 2. **RFC-4180 quoting:** the (possibly prefixed) value is wrapped in
///    double-quotes with any internal `"` doubled when it contains a comma,
///    double-quote, or newline.
///
/// Returned as a `Cow` to avoid allocating for the common (untouched) field.
fn csv_escape(field: &str) -> std::borrow::Cow<'_, str> {
    let needs_formula_guard = field
        .chars()
        .next()
        .is_some_and(|c| matches!(c, '=' | '+' | '-' | '@' | '\t' | '\r'));
    let needs_quoting = field.contains([',', '"', '\n', '\r']);

    if !needs_formula_guard && !needs_quoting {
        return std::borrow::Cow::Borrowed(field);
    }

    // Prefix the formula lead-in INSIDE any RFC-4180 quoting, so the cell's
    // literal text becomes `'=…`.
    let guarded = if needs_formula_guard {
        std::borrow::Cow::Owned(format!("'{field}"))
    } else {
        std::borrow::Cow::Borrowed(field)
    };
    if needs_quoting {
        std::borrow::Cow::Owned(format!("\"{}\"", guarded.replace('"', "\"\"")))
    } else {
        guarded
    }
}

#[cfg(test)]
#[path = "inquiry_tests.rs"]
mod inquiry_tests;
