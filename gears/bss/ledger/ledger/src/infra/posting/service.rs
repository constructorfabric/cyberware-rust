//! `PostingService` — the transactional posting engine. One ACID
//! transaction per entry runs the full sequence: idempotency claim → insert
//! balanced lines → fiscal-period gate → account lifecycle + `normal_side`
//! lookup → balance projection → finalize → COMMIT. Pure structural
//! invariants (`validate_balanced_entry`) and the line-count ceiling are
//! checked BEFORE the transaction opens, so a malformed request never takes
//! a write lock.
//!
//! ## Error handling across the transaction boundary
//!
//! [`DBProvider::transaction`] fixes the closure error type to
//! [`DbError`], yet a business rejection discovered AFTER a write (e.g. a
//! negative balance after the journal insert) MUST roll the transaction
//! back. The closure therefore encodes a business [`DomainError`] into a
//! sentinel [`DbError::Sea`] (`DbErr::Custom`) and returns `Err`, forcing a
//! rollback; once `transaction()` returns, the sentinel is decoded back into
//! the original [`DomainError`]. A `DbError` WITHOUT the sentinel prefix is a
//! genuine infrastructure fault and surfaces as [`DomainError::Internal`].

use std::collections::HashMap;
use std::sync::Arc;

use bss_ledger_sdk::{PostingRef, Side};
use sea_orm::DbErr;
use toolkit_db::secure::{AccessScope, DbTx, TxConfig};
use toolkit_db::{DBProvider, DbError};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::{NewEntry, NewLine};
use crate::domain::posting::{LineFacts, validate_balanced_entry};
use crate::domain::status::LIFECYCLE_OPEN;
use crate::infra::events::payloads::{
    AlarmCategory, AlarmSeverity, LedgerEntryPosted, LedgerInvariantAlarm, LedgerLineSummary,
};
use crate::infra::posting::chain::ChainSealer;
use crate::infra::posting::idempotency::{
    ClaimOutcome, IdempotencyGate, PostingRefRow, STATUS_POSTED, STATUS_QUEUED,
};
use crate::infra::posting::period::{FiscalPeriodGuard, PeriodError};
use crate::infra::posting::projector::{BalanceProjector, ProjectError};
use crate::infra::storage::repo::{JournalRepo, ReferenceRepo};
use time::OffsetDateTime;

/// Maximum number of lines a single entry may carry.
const MAX_LINES: usize = 1000;

/// Record-separator framing a sentinel-encoded business error inside a
/// `DbErr::Custom` payload: `LEDGER_POST_ERR␟<CODE>␟<message>`.
const SENTINEL_TAG: &str = "LEDGER_POST_ERR";
/// Unit-separator used inside the sentinel payload.
const SENTINEL_SEP: char = '\u{1f}';

/// Retry-extractor for the `SERIALIZABLE` post: a wrapped `DbErr` so a
/// serialization failure (surfaced at a statement or COMMIT) is recognised as
/// retryable contention. The business-rejection sentinel is a `DbErr::Custom`,
/// which the contention classifier treats as NON-retryable — so only genuine
/// conflicts retry, business rejections propagate immediately.
fn as_db_err(e: &DbError) -> Option<&sea_orm::DbErr> {
    match e {
        DbError::Sea(db_err) => Some(db_err),
        _ => None,
    }
}

/// The facts a finalized fresh post exposes to its in-transaction sidecar:
/// the new entry id and its DB-generated sequence. Passed by reference so the
/// sidecar can stamp counter rows that must commit atomically with the journal
/// entry.
#[derive(Clone, Copy, Debug)]
pub struct PostedFacts {
    pub entry_id: Uuid,
    pub created_seq: i64,
}

/// An in-transaction hook the posting engine runs AFTER balance projection and
/// BEFORE the dedup row is finalized, on the fresh-post path only (a replay
/// returns before projection). The sidecar writes counter rows (e.g. the
/// payment settlement / allocation caches) inside the SAME serializable
/// transaction, so they commit atomically with the entry or roll back with it.
///
/// An `Err(DomainError)` rolls the whole post back; the engine encodes it as a
/// non-retryable business sentinel so it surfaces to the caller unchanged.
/// Threaded as `Arc<dyn PostSidecar>` (not `&dyn`) because the retry closure is
/// `FnMut` and must `Clone` its inputs across attempts.
#[async_trait::async_trait]
pub trait PostSidecar: Send + Sync {
    /// Run the sidecar's in-transaction writes against the posted facts.
    ///
    /// # Errors
    /// A [`DomainError`] rolls the post back (encoded as a non-retryable
    /// business rejection).
    async fn run(
        &self,
        txn: &DbTx<'_>,
        scope: &AccessScope,
        posted: &PostedFacts,
    ) -> Result<(), DomainError>;

    /// Whether this sidecar must run BEFORE balance projection (default: `false`
    /// — project first, then the sidecar). A refund / claw-back sidecar
    /// overrides this to `true` so its rank-1 money-out cap / underflow CHECK
    /// runs first: an over-refund or out-of-order claw-back then surfaces as the
    /// canonical `RefundExceedsSettled` / `RefundClawbackDeferred`, instead of
    /// the projector's structural no-negative guard tripping first with a raw
    /// `NegativeBalance` (a stage-1 over-settled forward draws `UNALLOCATED`
    /// negative; an out-of-order claw-back draws `REFUND_CLEARING` negative —
    /// both before the cap classification can refine the error).
    fn run_before_projection(&self) -> bool {
        false
    }
}

/// The transactional posting engine.
#[derive(Clone)]
pub struct PostingService {
    db: DBProvider<DbError>,
    journal: JournalRepo,
    reference: ReferenceRepo,
    idempotency: IdempotencyGate,
    freeze: crate::infra::posting::freeze::TamperFreezeGuard,
    period: FiscalPeriodGuard,
    policy: crate::infra::policy_version::PolicyVersionGuard,
    projector: BalanceProjector,
    chain: ChainSealer,
    publisher: Arc<crate::infra::events::publisher::LedgerEventPublisher>,
}

/// How [`PostingService::post_in_txn`] step 1 (the idempotency gate) behaves —
/// the ONLY thing that differs between a fresh inline post and the deferred-apply
/// drive of a previously-queued item. Everything after step 1 (period gate →
/// insert → project → sidecar → finalize → outbox) is byte-identical for both
/// modes, so the body is parameterized by this rather than duplicated.
#[derive(Clone, Copy, Debug)]
enum ClaimMode {
    /// The unchanged inline-post path: `claim` the dedup row (seed `CLAIMED`) via
    /// `INSERT … ON CONFLICT DO NOTHING`; a conflict is a replay. This is exactly
    /// today's behaviour — `post()` always passes `Fresh`, so the public posting
    /// contract is unchanged.
    Fresh,
    /// The deferred-apply path (Group D): the dedup row was already claimed
    /// `QUEUED` at intake, so DON'T re-claim — `read` it instead. A `POSTED` row
    /// is an idempotent re-drive of an already-applied item (return `Replay`); a
    /// `QUEUED` row falls through to the SAME period-gate/insert/project/sidecar/
    /// finalize tail as `Fresh` — `finalize` flips `QUEUED → POSTED` exactly as it
    /// flips `CLAIMED → POSTED`. Any other state (no row / `CLAIMED`) is an
    /// invariant breach and surfaces as an infra fault.
    QueuedApply,
}

/// How `run_post` / `post_in_txn` claim the idempotency-dedup row: the
/// [`ClaimMode`] plus, for a `Fresh` claim, an optional REQUEST-based payload
/// hash to bind instead of the entry-derived one (so a payment orchestrator's
/// replay short-circuit can reject a same-key / different-payload reuse). A
/// `QueuedApply` carries no override — it reads the row claimed at intake.
/// Bundling the two lets the post entry points read as one intent
/// (`ClaimSpec::fresh()` / `::fresh_with_request_hash(h)` / `::queued_apply()`)
/// and keeps `run_post` to a tidy arity.
#[derive(Clone)]
struct ClaimSpec {
    mode: ClaimMode,
    /// `Fresh` only: a request-based hash to key the claim on instead of the
    /// entry-derived `payload_hash`. `None` ⇒ derive from the entry (the
    /// byte-identical pre-Group-D behaviour).
    payload_hash_override: Option<String>,
}

impl ClaimSpec {
    /// Inline post keyed on the entry-derived payload hash (today's `post()`).
    fn fresh() -> Self {
        Self {
            mode: ClaimMode::Fresh,
            payload_hash_override: None,
        }
    }

    /// Inline post keyed on an externally-computed REQUEST hash.
    fn fresh_with_request_hash(request_hash: String) -> Self {
        Self {
            mode: ClaimMode::Fresh,
            payload_hash_override: Some(request_hash),
        }
    }

    /// Deferred apply of a row already claimed `QUEUED` at intake.
    fn queued_apply() -> Self {
        Self {
            mode: ClaimMode::QueuedApply,
            payload_hash_override: None,
        }
    }
}

/// Outcome of the in-transaction posting body, carried out of the closure on
/// the commit (`Ok`) path.
#[derive(Clone, Copy, Debug)]
enum PostOutcome {
    /// A fresh post: the new entry id + DB-generated sequence.
    Posted {
        entry_id: uuid::Uuid,
        created_seq: i64,
    },
    /// An idempotent replay of a prior, finalized post (its real entry id).
    Replay { entry_id: uuid::Uuid },
}

impl PostingService {
    /// Build a `PostingService` and its sub-repositories from one provider.
    #[must_use]
    pub fn new(
        db: DBProvider<DbError>,
        publisher: Arc<crate::infra::events::publisher::LedgerEventPublisher>,
    ) -> Self {
        let journal = JournalRepo::new(db.clone());
        let reference = ReferenceRepo::new(db.clone());
        Self {
            db,
            journal,
            reference,
            idempotency: IdempotencyGate::new(),
            freeze: crate::infra::posting::freeze::TamperFreezeGuard::new(),
            period: FiscalPeriodGuard::new(),
            policy: crate::infra::policy_version::PolicyVersionGuard::new(),
            projector: BalanceProjector::new(),
            chain: ChainSealer::new(),
            publisher,
        }
    }

    /// Post a balanced entry in one ACID transaction, idempotent on the
    /// `(tenant, source_doc_type, source_business_id)` key.
    ///
    /// # Errors
    /// A [`DomainError`] on any domain rejection (unbalanced/empty/mixed-payer,
    /// too-large, idempotency conflict, period-closed, account-closed,
    /// negative-balance), or [`DomainError::Internal`] on an infrastructure
    /// fault.
    pub async fn post(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        entry: NewEntry,
        lines: Vec<NewLine>,
        sidecar: Option<Arc<dyn PostSidecar>>,
    ) -> Result<PostingRef, DomainError> {
        // The unchanged inline-post path: `ClaimMode::Fresh` makes step 1 of the
        // in-txn body `claim` the dedup row exactly as before, so this is
        // byte-identical to the pre-Group-D `post`.
        self.run_post(ctx, scope, entry, lines, sidecar, ClaimSpec::fresh())
            .await
    }

    /// Like [`Self::post`] but binds an externally-computed, REQUEST-based payload
    /// hash to the idempotency claim instead of deriving the entry-based hash. A
    /// payment orchestrator (allocate / credit) that short-circuits a replay
    /// BEFORE rebuilding its (state-dependent) entry must key dedup on a hash that
    /// is stable across that rebuild — derived from the request, not the entry —
    /// so the short-circuit can compare it and reject a same-key / different-
    /// payload reuse as [`DomainError::IdempotencyConflict`] rather than silently
    /// replaying the prior result. Otherwise identical to [`Self::post`]
    /// (`ClaimMode::Fresh`).
    ///
    /// # Errors
    /// As [`Self::post`].
    pub async fn post_with_request_hash(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        entry: NewEntry,
        lines: Vec<NewLine>,
        sidecar: Option<Arc<dyn PostSidecar>>,
        request_hash: String,
    ) -> Result<PostingRef, DomainError> {
        self.run_post(
            ctx,
            scope,
            entry,
            lines,
            sidecar,
            ClaimSpec::fresh_with_request_hash(request_hash),
        )
        .await
    }

    /// Post a previously-QUEUED allocation as a deferred apply (Group D) — a
    /// sibling of [`Self::post`] with IDENTICAL pre-txn validation and the SAME
    /// serializable retry wrapper, differing only in the dedup gate: step 1
    /// `read`s the dedup row (claimed `QUEUED` at intake) instead of re-claiming
    /// it, then finalizes `QUEUED → POSTED`. An already-`POSTED` row is an
    /// idempotent re-drive and returns a replay. Caps are thus re-evaluated AT
    /// APPLY TIME (§4.7) — the period gate, the projector's no-negative guard, and
    /// the sidecar's per-payment cap CHECK all run against then-current state, not
    /// intake-time state.
    ///
    /// The queue row's `→APPLIED` flip is NOT done here — it rides the `sidecar`
    /// (the caller composes the allocation sidecar with the queue-flip), so the
    /// apply effect and the work-state transition commit atomically in this one
    /// transaction.
    ///
    /// # Errors
    /// A [`DomainError`] on any domain rejection (re-evaluated cap / period /
    /// account / negative-balance), or [`DomainError::Internal`] on an infra
    /// fault — including a dedup row that is neither `QUEUED` nor a finalized
    /// `POSTED` (an invariant breach for a queued-apply).
    pub async fn post_queued_apply(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        entry: NewEntry,
        lines: Vec<NewLine>,
        sidecar: Option<Arc<dyn PostSidecar>>,
    ) -> Result<PostingRef, DomainError> {
        self.run_post(ctx, scope, entry, lines, sidecar, ClaimSpec::queued_apply())
            .await
    }

    /// Shared driver behind [`Self::post`] / [`Self::post_with_request_hash`]
    /// (`Fresh`) and [`Self::post_queued_apply`] (`QueuedApply`): the pre-txn
    /// validation + `normal_side` load + the SERIALIZABLE retry transaction + the
    /// out-of-band invariant alarm. The modes differ only in the [`ClaimSpec`]:
    /// its [`ClaimMode`] is threaded into the in-txn body's step 1, and its
    /// optional request-hash override keys a `Fresh` claim (`None` reproduces the
    /// byte-identical pre-Group-D inline `post`).
    async fn run_post(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        entry: NewEntry,
        lines: Vec<NewLine>,
        sidecar: Option<Arc<dyn PostSidecar>>,
        claim: ClaimSpec,
    ) -> Result<PostingRef, DomainError> {
        // --- PRE-TRANSACTION (fail fast, no writes) ---
        let facts: Vec<LineFacts> = lines
            .iter()
            .map(|l| LineFacts {
                side: l.side,
                amount_minor: l.amount_minor,
                currency: l.currency.clone(),
                currency_scale: l.currency_scale,
                payer_tenant_id: l.payer_tenant_id,
                functional_amount_minor: l.functional_amount_minor,
            })
            .collect();
        if let Err(v) = validate_balanced_entry(&entry.entry_currency, &facts) {
            return Err(v.into());
        }
        if lines.len() > MAX_LINES {
            return Err(DomainError::EntryTooLarge(format!(
                "entry has {} lines (max {MAX_LINES})",
                lines.len()
            )));
        }

        // Pre-transaction gate: tenant-termination kill switch (design §3.2). A
        // held `tenant_posting_lock` refuses every post for the tenant with
        // `TENANT_POSTING_LOCKED`, before any write. Read on its own connection
        // (like the account-lifecycle pre-check below); a lock set CONCURRENTLY
        // is not caught here, tolerable for a rare admin op.
        if self
            .reference
            .is_tenant_posting_locked(scope, entry.tenant_id)
            .await
            .map_err(|e| decode_post_error(&repo_to_db(e)))?
        {
            return Err(DomainError::TenantPostingLocked(format!(
                "tenant {} is posting-locked",
                entry.tenant_id
            )));
        }

        // Pre-transaction gate: clock-skew guard (design §3.2 FiscalPeriodGuard).
        // Skew beyond ±24 h between the post's `posted_at_utc` and the server
        // wall clock is quarantined (`CLOCK_SKEW_QUARANTINE`), re-submittable via
        // the material-backdating exception path; skew beyond ±15 min posts but
        // raises a `CLOCK_SKEW` Warn alarm out-of-band (no rollback).
        match crate::infra::posting::period::classify_clock_skew(
            entry.posted_at_utc,
            OffsetDateTime::now_utc(),
        ) {
            crate::infra::posting::period::ClockSkewVerdict::Ok => {}
            crate::infra::posting::period::ClockSkewVerdict::Warn => {
                tracing::warn!(
                    tenant_id = %entry.tenant_id,
                    posted_at_utc = %entry.posted_at_utc,
                    "clock skew beyond ±15 min on a post"
                );
                let alarm = LedgerInvariantAlarm {
                    category: AlarmCategory::ClockSkew,
                    severity: crate::infra::events::alarm_catalog::severity(
                        AlarmCategory::ClockSkew,
                    ),
                    tenant_id: entry.tenant_id,
                    scope: format!(
                        "tenant:{}/flow:{}/business:{}",
                        entry.tenant_id,
                        entry.source_doc_type.as_str(),
                        entry.source_business_id
                    ),
                    code: "CLOCK_SKEW".to_owned(),
                    detail: format!(
                        "posted_at_utc {} skewed >±15 min from server clock",
                        entry.posted_at_utc
                    ),
                    affected: Vec::new(),
                };
                self.publisher.emit_invariant_alarm(ctx, alarm).await;
            }
            crate::infra::posting::period::ClockSkewVerdict::Reject => {
                return Err(DomainError::ClockSkewQuarantine(format!(
                    "posted_at_utc {} skewed >±24 h from server clock",
                    entry.posted_at_utc
                )));
            }
        }

        // Account lifecycle + normal_sides are read BEFORE the transaction:
        // the repos open their own (non-transactional) connection, which
        // Postgres forbids inside an active transaction. A closed account fails
        // fast here with no writes. ACCEPTED LIMITATION: this read is outside the
        // serializable snapshot, so an account closed CONCURRENTLY (after this
        // read, before COMMIT) is not detected — unlike the in-txn period gate.
        // Tolerable: account close is a rare admin op and `normal_side` is
        // immutable; an in-txn account re-pin is a tracked follow-up.
        let normal_sides = self
            .load_normal_sides(scope, &lines)
            .await
            .map_err(|e| decode_post_error(&e))?;

        // Capture alarm-scope fields before `entry`/`lines` move into the
        // closure (same reason `ctx` is cloned below) — used out-of-band on
        // the Err path; internal ids only, no PII.
        let alarm_tenant = entry.tenant_id;
        let alarm_flow = entry.source_doc_type.as_str().to_owned();
        let alarm_business = entry.source_business_id.clone();

        // --- TRANSACTION ---
        // Clone the engine + scope into the closure so the `transaction`
        // borrow of `self.db` does not conflict with the captured engine.
        let svc = self.clone();
        let scope = scope.clone();
        // Owned clone for the `move` closure; the borrowed `ctx` param stays
        // usable on the Err arm below (out-of-band alarm).
        let ctx_txn = ctx.clone();
        // SERIALIZABLE + retry: period close runs SERIALIZABLE too, so an
        // overlapping post/close pair conflicts under Postgres SSI and the loser
        // retries here — close can never certify a period this entry lands in.
        // The body is `FnMut` (re-clones its inputs per attempt); the post is
        // idempotent across attempts (the dedup claim re-runs from a fresh txn).
        // Business rejections carry a non-retryable sentinel `DbErr`, so only
        // genuine contention retries.
        let result = self
            .db
            .db()
            .transaction_with_retry(TxConfig::serializable(), as_db_err, move |txn| {
                let svc = svc.clone();
                let ctx_txn = ctx_txn.clone();
                let scope = scope.clone();
                let entry = entry.clone();
                let lines = lines.clone();
                let normal_sides = normal_sides.clone();
                // `Arc<dyn PostSidecar>` is cheap to clone per attempt; the
                // sidecar's writes re-run from a fresh txn on a retry, mirroring
                // the idempotent post body.
                let sidecar = sidecar.clone();
                // `ClaimSpec` is re-cloned per attempt (like `entry`/`lines`),
                // since the retry body is `FnMut` and it carries an owned hash.
                let claim = claim.clone();
                Box::pin(async move {
                    svc.post_in_txn(
                        &ctx_txn,
                        txn,
                        &scope,
                        entry,
                        lines,
                        normal_sides,
                        sidecar,
                        claim,
                    )
                    .await
                })
            })
            .await;

        match result {
            Ok(PostOutcome::Posted {
                entry_id,
                created_seq,
            }) => Ok(PostingRef {
                entry_id,
                created_seq,
                replayed: false,
            }),
            Ok(PostOutcome::Replay { entry_id }) => Ok(PostingRef {
                // A replay carries the prior, finalized entry id (the in-txn
                // body only yields Replay once it has confirmed the dedup row
                // is POSTED with a real id — never the nil UUID). The sequence
                // is not re-read here; replay callers key on the id.
                entry_id,
                created_seq: 0,
                replayed: true,
            }),
            Err(db_err) => {
                let err = decode_post_error(&db_err);
                // Out-of-band invariant alarm: fire-and-forget on a separate
                // committed connection, so it survives the rolled-back post and
                // never changes the error returned to the caller.
                if let Some((category, severity, code)) = alarm_for(&err) {
                    let alarm = LedgerInvariantAlarm {
                        category,
                        severity,
                        tenant_id: alarm_tenant,
                        scope: format!(
                            "tenant:{alarm_tenant}/flow:{alarm_flow}/business:{alarm_business}"
                        ),
                        code: code.to_owned(),
                        detail: err.to_string(), // internal diagnostic — no PII
                        // Hot posting-path rejection: the single offending entry
                        // is named in `scope`/`detail`; no per-grain list.
                        affected: Vec::new(),
                    };
                    self.publisher.emit_invariant_alarm(ctx, alarm).await;
                }
                Err(err)
            }
        }
    }

    /// The in-transaction posting body. Business rejections are encoded as a
    /// sentinel `DbError` so the closure error type stays `DbError` while
    /// still forcing a rollback.
    // Threads the post's distinct inputs (entry / lines / normal_sides / sidecar)
    // through the serializable retry closure; the claim mode + request-hash are
    // bundled into `ClaimSpec`, so this stays just over the lint's arg ceiling.
    #[allow(clippy::too_many_arguments)]
    async fn post_in_txn(
        &self,
        ctx: &SecurityContext,
        txn: &toolkit_db::secure::DbTx<'_>,
        scope: &AccessScope,
        entry: NewEntry,
        lines: Vec<NewLine>,
        normal_sides: HashMap<uuid::Uuid, Side>,
        sidecar: Option<Arc<dyn PostSidecar>>,
        claim: ClaimSpec,
    ) -> Result<PostOutcome, DbError> {
        let ClaimSpec {
            mode: claim_mode,
            payload_hash_override,
        } = claim;
        let tenant = entry.tenant_id;
        let legal_entity = entry.legal_entity_id;
        let flow = entry.source_doc_type.as_str().to_owned();
        let business_id = entry.source_business_id.clone();
        let period_id = entry.period_id.clone();

        // 1. + 2. Idempotency gate. `Fresh` claims the dedup row (the unchanged
        // inline path); `QueuedApply` reads the row already claimed `QUEUED` at
        // intake. Both yield "proceed" (fall through to step 3) or an early
        // `Replay`; only the gate mechanics differ, so the whole tail below is
        // shared. Only `Fresh` needs the payload hash (the claim + the replay's
        // conflict guard); `QueuedApply` reads the row and re-derives the entry
        // from the same queued payload, so the hash is computed lazily in the
        // `Fresh` arm.
        match claim_mode {
            ClaimMode::Fresh => {
                // `Fresh` keys the dedup claim on this hash; a payment orchestrator
                // may override it with a REQUEST-based hash (stable across its
                // state-dependent entry rebuild) so its replay short-circuit can
                // compare against it.
                let payload_hash = payload_hash_override
                    .unwrap_or_else(|| IdempotencyGate::payload_hash(&entry, &lines));
                match self
                    .idempotency
                    .claim(txn, tenant, &flow, &business_id, &payload_hash)
                    .await
                    .map_err(repo_to_db)?
                {
                    ClaimOutcome::Replay(row) => {
                        if row.payload_hash != payload_hash {
                            return Err(business(DomainError::IdempotencyConflict(
                                "idempotency key reused with a different payload".to_owned(),
                            )));
                        }
                        // A matching-hash replay must reference a FINALIZED post.
                        // The winner's claim + finalize are atomic in one
                        // transaction, and the conflicting `INSERT … ON CONFLICT DO
                        // NOTHING` blocks a concurrent claimer until that
                        // transaction commits — so a committed dedup row is always
                        // POSTED with a result id. Guard the invariant: never hand
                        // back the nil UUID for a row that is somehow not
                        // finalized; surface an infra fault so the caller retries
                        // instead of keying on a phantom entry.
                        return match row.result_entry_id {
                            Some(entry_id) if row.status == STATUS_POSTED => {
                                Ok(PostOutcome::Replay { entry_id })
                            }
                            _ => Err(infra(format!(
                                "idempotency replay race: dedup row {tenant}/{flow}/{business_id} \
                                 not finalized (status={}, has_result={})",
                                row.status,
                                row.result_entry_id.is_some()
                            ))),
                        };
                    }
                    ClaimOutcome::Claimed => {}
                }
            }
            ClaimMode::QueuedApply => {
                // The dedup row was claimed `QUEUED` at intake — DON'T re-claim,
                // `read` it in-txn. A `POSTED` row with a result id is an
                // idempotent re-drive of an already-applied item (return the prior
                // entry as a replay — the queue-flip sidecar is a no-op a second
                // time since the row is already terminal, but a re-drive normally
                // never reaches here because the queue row is no longer claimable).
                // A `QUEUED` row falls through to the shared tail (period gate →
                // insert → project → sidecar → finalize), and `finalize` flips it
                // `QUEUED → POSTED` exactly as it flips `CLAIMED`. Anything else (no
                // row, or `CLAIMED`) is an invariant breach for a queued apply.
                match self
                    .idempotency
                    .read(txn, tenant, &flow, &business_id)
                    .await
                    .map_err(repo_to_db)?
                {
                    Some(PostingRefRow {
                        status,
                        result_entry_id: Some(entry_id),
                        ..
                    }) if status == STATUS_POSTED => {
                        return Ok(PostOutcome::Replay { entry_id });
                    }
                    Some(row) if row.status == STATUS_QUEUED => {}
                    other => {
                        return Err(infra(format!(
                            "queued-apply on a non-QUEUED dedup row {tenant}/{flow}/{business_id} \
                             (status={}, has_result={})",
                            other.as_ref().map_or("<absent>", |r| r.status.as_str()),
                            other.as_ref().is_some_and(|r| r.result_entry_id.is_some()),
                        )));
                    }
                }
            }
        }

        // 2b. Tamper-freeze gate (fail fast, BEFORE any write): if the integrity
        // verifier froze this scope (a broken tamper-evidence chain), reject a
        // FRESH post into it with `TamperVerificationFailed` before the
        // append-only insert takes write locks. A replay returned above, so it
        // is never blocked; the freeze read is in-txn, so a concurrent
        // freeze/post pair conflicts under SSI like the period gate.
        self.freeze.check(txn, scope, tenant, &period_id).await?;

        // 3. Fiscal-period gate (fail fast, BEFORE any write): a post into a
        // CLOSED/absent period is rejected here, before the append-only insert
        // takes write locks on journal_entry/journal_line. pin_open reads the
        // period row in-txn, so the post/close SSI conflict is unchanged.
        match self
            .period
            .pin_open(txn, tenant, legal_entity, &period_id)
            .await
        {
            Ok(()) => {}
            Err(PeriodError::Closed) => {
                return Err(business(DomainError::PeriodClosed(
                    "fiscal period is closed or absent".to_owned(),
                )));
            }
            Err(PeriodError::Db(e)) => return Err(infra(format!("period guard: {e}"))),
        }

        // Keep clones for the projector (the insert consumes the originals).
        let entry_for_proj = entry.clone();
        let lines_for_proj = lines.clone();

        // 3b. Policy-version guard (§4.6, AC #15): a correction (one whose header
        // carries `reverses_entry_id`) MUST REUSE the original posting's pinned
        // evidence refs, never invent new ones. Read-only over the original's
        // lines, so it slots in after the period gate and before the insert — it
        // takes no write lock. A fresh original posting (`reverses_entry_id` None)
        // is a no-op fast-exit. A correction that invents a pinned ref the
        // original never had is rejected with `PolicyVersionViolation`.
        self.policy
            .check(txn, scope, &entry_for_proj, &lines_for_proj)
            .await?;

        // 4. Append-only insert of the header + lines.
        let entry_ref = self
            .journal
            .insert_entry_with_lines(txn, entry, lines)
            .await
            .map_err(repo_to_db)?;

        // 6 / 6b. Balance projection + in-transaction sidecar (non-replay path
        // only — a replay, in either `ClaimMode`, returns above). Their ORDER
        // depends on the sidecar: a refund / claw-back sidecar
        // (`run_before_projection() == true`) runs its rank-1 money-out cap /
        // underflow CHECK BEFORE projection, so an over-refund / out-of-order
        // claw-back surfaces as the canonical `RefundExceedsSettled` /
        // `RefundClawbackDeferred` instead of the projector's structural
        // no-negative guard tripping first with a raw `NegativeBalance`. Every
        // other post projects first, then runs its sidecar. Either way both run
        // AFTER the insert and BEFORE finalize, committing atomically with the
        // journal entry; an `Err` is encoded as a non-retryable business sentinel
        // that rolls the whole post back. On the `QueuedApply` path the
        // (project-first) sidecar marks the queue row `→APPLIED` alongside the
        // allocation counter writes.
        let posted_facts = PostedFacts {
            entry_id: entry_ref.entry_id,
            created_seq: entry_ref.created_seq,
        };
        let sidecar_before = sidecar
            .as_ref()
            .is_some_and(|sc| sc.run_before_projection());

        if sidecar_before && let Some(sc) = &sidecar {
            sc.run(txn, scope, &posted_facts).await.map_err(business)?;
        }

        self.projector
            .project(
                txn,
                scope,
                &entry_for_proj,
                &lines_for_proj,
                &normal_sides,
                entry_ref.created_seq,
            )
            .await
            .map_err(project_to_db)?;

        if !sidecar_before && let Some(sc) = &sidecar {
            sc.run(txn, scope, &posted_facts).await.map_err(business)?;
        }

        // 6c. Seal the tamper-evidence chain: link this entry onto the tenant's
        // tip (or genesis) and advance the tip. The single from-NULL chain-only
        // UPDATE is the one mutation the relaxed append-only trigger permits.
        // A failure rolls the whole post back via `?` like the other steps.
        self.chain
            .seal(txn, scope, &entry_for_proj, &lines_for_proj, &entry_ref)
            .await?;

        // 7. Finalize the dedup row before COMMIT — flips `CLAIMED → POSTED`
        // (Fresh) or `QUEUED → POSTED` (QueuedApply); the update is keyed on the
        // PK and is status-agnostic, so it stamps the result id either way.
        self.idempotency
            .finalize(
                txn,
                tenant,
                &flow,
                &business_id,
                entry_ref.entry_id,
                entry_ref.created_seq,
            )
            .await
            .map_err(repo_to_db)?;

        // 8. Publish `billing.ledger.entry.posted` into the SAME transaction
        // (transactional outbox): the event row commits atomically with the
        // entry, or a publish failure rolls the whole post back via `infra`.
        // Non-replay path only — a replay (either mode) returns above without
        // publishing (the entry was already announced on its first post).
        let posted_event = LedgerEntryPosted {
            entry_id: entry_ref.entry_id,
            tenant_id: entry_for_proj.tenant_id,
            legal_entity_id: entry_for_proj.legal_entity_id,
            period_id: entry_for_proj.period_id.clone(),
            source_doc_type: entry_for_proj.source_doc_type.as_str().to_owned(),
            source_business_id: entry_for_proj.source_business_id.clone(),
            posted_at_utc: entry_for_proj.posted_at_utc,
            created_seq: entry_ref.created_seq,
            lines: lines_for_proj
                .iter()
                .map(|l| LedgerLineSummary {
                    account_class: l.account_class.as_str().to_owned(),
                    side: l.side.as_str().to_owned(),
                    amount_minor: l.amount_minor,
                    currency: l.currency.clone(),
                    currency_scale: l.currency_scale,
                })
                .collect(),
        };
        self.publisher
            .publish_entry_posted(ctx, txn, posted_event)
            .await
            .map_err(|e| infra(format!("{e}")))?;

        // 9. COMMIT happens when the closure returns Ok.
        Ok(PostOutcome::Posted {
            entry_id: entry_ref.entry_id,
            created_seq: entry_ref.created_seq,
        })
    }

    /// Load each DISTINCT account's `normal_side`, asserting it is provisioned
    /// and `OPEN`. Absent or non-OPEN → an encoded `AccountClosed` business
    /// error.
    async fn load_normal_sides(
        &self,
        scope: &AccessScope,
        lines: &[NewLine],
    ) -> Result<HashMap<uuid::Uuid, Side>, DbError> {
        let mut normal_sides: HashMap<uuid::Uuid, Side> = HashMap::new();
        for line in lines {
            if normal_sides.contains_key(&line.account_id) {
                continue;
            }
            let account = self
                .reference
                .find_account(scope, line.account_id)
                .await
                .map_err(repo_to_db)?;
            let Some(account) = account else {
                return Err(business(DomainError::AccountClosed(format!(
                    "account {} is not provisioned",
                    line.account_id
                ))));
            };
            if account.lifecycle_state != LIFECYCLE_OPEN {
                return Err(business(DomainError::AccountClosed(format!(
                    "account {} is not OPEN",
                    line.account_id
                ))));
            }
            let side = match account.normal_side.as_str() {
                "DR" => Side::Debit,
                "CR" => Side::Credit,
                other => {
                    return Err(infra(format!(
                        "account {} has an invalid normal_side {other:?}",
                        line.account_id
                    )));
                }
            };
            normal_sides.insert(line.account_id, side);
        }
        Ok(normal_sides)
    }
}

/// Map a [`RepoError`](crate::domain::model::RepoError) into the sentinel
/// `DbError`: a repo db/row failure is an infrastructure fault; a
/// scale-locked / out-of-range rejection maps to its domain variant.
pub(crate) fn repo_to_db(e: crate::domain::model::RepoError) -> DbError {
    use crate::domain::model::RepoError;
    match e {
        RepoError::CurrencyScaleLocked(c) => business(DomainError::CurrencyScaleLocked(format!(
            "currency scale locked: {c}"
        ))),
        // A wrong per-line scale changes the implied magnitude → out-of-range
        // (wire `AMOUNT_OUT_OF_RANGE`), preserving the prior posting contract.
        RepoError::ScaleOutOfRange(c) => business(DomainError::AmountOutOfRange(format!(
            "scale out of range: {c}"
        ))),
        other => infra(other.to_string()),
    }
}

/// Map a [`ProjectError`] into the sentinel `DbError`.
fn project_to_db(e: ProjectError) -> DbError {
    match e {
        ProjectError::NegativeBalance {
            account_id,
            balance_minor,
        } => business(DomainError::NegativeBalance(format!(
            "balance for account {account_id} would go negative ({balance_minor})"
        ))),
        // Should not happen — every account's side is loaded in step 5.
        ProjectError::MissingNormalSide(id) => business(DomainError::AccountClosed(format!(
            "missing normal_side for account {id}"
        ))),
        // Invariant breach (builders always set the bucket) — surface as Internal
        // rather than silently keying a phantom "" wallet sub-balance.
        ProjectError::MissingCreditEventType(id) => business(DomainError::Internal(format!(
            "REUSABLE_CREDIT line {id} missing credit_grant_event_type"
        ))),
        // A coalesced money delta overflowed i64 — a clean amount-class rejection
        // (422), not a 500: surface as the business error the adjustments path
        // already uses for out-of-range amounts.
        ProjectError::Overflow {
            account_id,
            currency,
            field,
        } => business(DomainError::AmountOutOfRange(format!(
            "coalesced money delta overflowed i64 for account {account_id} ({currency}, {field})"
        ))),
        ProjectError::Db(e) => infra(format!("projector: {e}")),
    }
}

/// Encode a business [`DomainError`] as a sentinel `DbError` so the transaction
/// closure (whose error type is fixed to `DbError`) rolls back yet preserves
/// the rejection for decoding after `transaction()` returns. The payload is a
/// `DbErr::Custom`, which the contention classifier treats as NON-retryable —
/// so a business rejection propagates immediately and is never retried.
pub(crate) fn business(err: DomainError) -> DbError {
    let (tag, detail) = domain_parts(err);
    DbError::Sea(DbErr::Custom(format!(
        "{SENTINEL_TAG}{SENTINEL_SEP}{tag}{SENTINEL_SEP}{detail}"
    )))
}

/// Encode an internal (infrastructure) failure as a non-sentinel `DbError`.
pub(crate) fn infra(message: impl Into<String>) -> DbError {
    DbError::Sea(DbErr::Custom(message.into()))
}

/// Decode a `DbError` returned from a `transaction_with_retry` back into a
/// [`DomainError`]: a sentinel-tagged `DbErr::Custom` (written by [`business`])
/// yields the original business rejection; any other `DbError` is an
/// infrastructure fault ([`DomainError::Internal`]). Shared by service paths
/// that run their own sentinel-carrying transaction (e.g. the audit-surface
/// cross-tenant elevation txn) and need the post path's decode semantics.
pub(crate) fn decode_business_error(db_err: &DbError) -> DomainError {
    decode_post_error(db_err)
}

/// Decode a `DbError` returned from `transaction()` back into a [`DomainError`]:
/// a sentinel-tagged `DbErr::Custom` yields the original business rejection; any
/// other `DbError` is an infrastructure fault ([`DomainError::Internal`]).
fn decode_post_error(db_err: &DbError) -> DomainError {
    if let DbError::Sea(DbErr::Custom(payload)) = db_err
        && let Some(rest) = payload.strip_prefix(&format!("{SENTINEL_TAG}{SENTINEL_SEP}"))
        && let Some((tag, detail)) = rest.split_once(SENTINEL_SEP)
    {
        return domain_from_parts(tag, detail.to_owned());
    }
    // A concurrent wallet over-draw that slips past the app-level pre-check trips
    // the DB no-negative CHECK on reusable_credit_subbalance; surface it as the
    // clean CreditExceedsWallet (→409) rather than an opaque Internal (500). No
    // money is lost (the CHECK held); only the wire surface is corrected.
    if db_err
        .to_string()
        .contains("chk_reusable_credit_subbalance_no_negative")
    {
        return DomainError::CreditExceedsWallet(
            "concurrent wallet over-draw rejected by the no-negative guard".to_owned(),
        );
    }
    DomainError::Internal(db_err.to_string())
}

/// Map a decoded [`DomainError`] to its out-of-band invariant alarm
/// (category, severity, wire code), or `None` for ordinary client rejections
/// that raise no alarm.
fn alarm_for(err: &DomainError) -> Option<(AlarmCategory, AlarmSeverity, &'static str)> {
    // Only the (category, wire code) pair is named per variant; the severity is
    // ALWAYS taken from the normative §4.7 catalog (`alarm_catalog::severity`) so
    // the emitter can never drift from it (e.g. an idempotency-key collision is
    // Critical per AC #19, not Warn).
    let (category, code) = match err {
        DomainError::IdempotencyConflict(_) => (
            AlarmCategory::IdempotencyPayloadConflict,
            "IDEMPOTENCY_PAYLOAD_CONFLICT",
        ),
        DomainError::NegativeBalance(_) => (
            AlarmCategory::NegativeBalanceViolation,
            "NEGATIVE_BALANCE_VIOLATION",
        ),
        // The per-schedule over-recognition cap CHECK rejected a release (Slice 4
        // §4.3 / §6): the cumulative recognized would exceed the deferred total.
        // Fires out-of-band on the rolled-back release, exactly like the
        // negative-balance guard (the recognition stamp sidecar surfaces this as
        // `OverRecognition`). Severity is taken from the §4.7 catalog (Critical).
        DomainError::OverRecognition(_) => (AlarmCategory::OverRecognition, "OVER_RECOGNITION"),
        _ => return None,
    };
    Some((
        category,
        crate::infra::events::alarm_catalog::severity(category),
        code,
    ))
}

/// Split a [`DomainError`] into a stable per-variant tag + its detail for the
/// sentinel round-trip. Exhaustive, so a new variant forces a tag here; the
/// tags are internal to the post txn (encoded and decoded in this module only).
fn domain_parts(err: DomainError) -> (&'static str, String) {
    use DomainError as D;
    match err {
        D::Unbalanced(d) => ("Unbalanced", d),
        D::Empty(d) => ("Empty", d),
        D::MixedPayer(d) => ("MixedPayer", d),
        D::MissingPayer(d) => ("MissingPayer", d),
        D::MixedLegalEntity(d) => ("MixedLegalEntity", d),
        D::InconsistentScale(d) => ("InconsistentScale", d),
        D::AmountOutOfRange(d) => ("AmountOutOfRange", d),
        D::EntryTooLarge(d) => ("EntryTooLarge", d),
        D::InvalidRequest(d) => ("InvalidRequest", d),
        D::ScaleOutOfRange(d) => ("ScaleOutOfRange", d),
        D::CreditResidualUndisposed(d) => ("CreditResidualUndisposed", d),
        D::MoneyOutCapExceeded(d) => ("MoneyOutCapExceeded", d),
        D::AllocationTooLarge(d) => ("AllocationTooLarge", d),
        D::AllocationCurrencyMismatch(d) => ("AllocationCurrencyMismatch", d),
        D::CurrencyMismatch(d) => ("CurrencyMismatch", d),
        // FX rate errors are raised pre-post (rate-lock); listed for the
        // exhaustive-match contract only (they never ride the sentinel).
        D::FxRateUnavailable(d) => ("FxRateUnavailable", d),
        D::FxRateStaleNotAllowed(d) => ("FxRateStaleNotAllowed", d),
        D::AllocationSplitInvalid(d) => ("AllocationSplitInvalid", d),
        D::GrantExceedsUnallocated(d) => ("GrantExceedsUnallocated", d),
        D::CreditExceedsOpenAr(d) => ("CreditExceedsOpenAr", d),
        D::CreditExceedsWallet(d) => ("CreditExceedsWallet", d),
        D::ScheduleTooLong(d) => ("ScheduleTooLong", d),
        D::SspSnapshotRequired(d) => ("SspSnapshotRequired", d),
        D::MissingPoAllocationGroup(d) => ("MissingPoAllocationGroup", d),
        D::RecognitionPolicyConflict(d) => ("RecognitionPolicyConflict", d),
        D::CreditNoteSplitAmbiguous(d) => ("CreditNoteSplitAmbiguous", d),
        D::CreditNoteExceedsHeadroom(d) => ("CreditNoteExceedsHeadroom", d),
        // The refund cap CHECKs fire INSIDE the post txn (the RefundPostSidecar's
        // counter increments), so these ride the sentinel to surface to the caller.
        D::RefundExceedsSettled(d) => ("RefundExceedsSettled", d),
        D::RefundExceedsAllocated(d) => ("RefundExceedsAllocated", d),
        D::ModificationTreatmentReview(d) => ("ModificationTreatmentReview", d),
        D::RecognitionWithoutInvoiceLink(d) => ("RecognitionWithoutInvoiceLink", d),
        D::PiiInMetadataValue(d) => ("PiiInMetadataValue", d),
        D::MissingInvestigationReason(d) => ("MissingInvestigationReason", d),
        D::CrossTenantAccessDenied(d) => ("CrossTenantAccessDenied", d),
        // Governed manual adjustment rejected by the §4.6 governor (allow-list /
        // write-off guard). Decided BEFORE the post (the handler runs `govern`
        // out-of-txn), so it never actually rides the sentinel — listed for the
        // exhaustive match contract.
        D::ManualAdjustmentNotAllowed(d) => ("ManualAdjustmentNotAllowed", d),
        D::PeriodClosed(d) => ("PeriodClosed", d),
        D::AccountClosed(d) => ("AccountClosed", d),
        D::PayerClosed(d) => ("PayerClosed", d),
        D::AccountMappingMissing(d) => ("AccountMappingMissing", d),
        D::NegativeBalance(d) => ("NegativeBalance", d),
        D::SettlementReturnOverAllocated(d) => ("SettlementReturnOverAllocated", d),
        D::InvalidDisputeTransition(d) => ("InvalidDisputeTransition", d),
        D::ChargebackExceedsSettled(d) => ("ChargebackExceedsSettled", d),
        D::ChargebackOnRefunded(d) => ("ChargebackOnRefunded", d),
        D::ClockSkewQuarantine(d) => ("ClockSkewQuarantine", d),
        D::PeriodNotOpen(d) => ("PeriodNotOpen", d),
        D::PeriodCloseBlocked(d) => ("PeriodCloseBlocked", d),
        D::PeriodCloseInProgress(d) => ("PeriodCloseInProgress", d),
        D::IdempotencyConflict(d) => ("IdempotencyConflict", d),
        D::CurrencyScaleLocked(d) => ("CurrencyScaleLocked", d),
        D::OverRecognition(d) => ("OverRecognition", d),
        // Group E: a claw-back whose money-out decrement would underflow is raised
        // by the refund post sidecar and MUST round-trip unchanged (the handler
        // matches on it to DEFER the claw-back to the queue, not hard-fail).
        D::RefundClawbackDeferred(d) => ("RefundClawbackDeferred", d),
        // Cross-currency unsupported-op reject (Slice 5): guarded BEFORE the post
        // (claw-back in the refund handler, mapping-correction in the REST handler),
        // so it never rides the sentinel — listed for the exhaustive match contract.
        D::FxOperationUnsupported(d) => ("FxOperationUnsupported", d),
        // The dispute-hold gate runs OUT-OF-TXN in the refund handler BEFORE the
        // post (the open dispute is read out-of-txn), so it never actually rides the
        // sentinel; listed for the exhaustive match contract (Z5-2).
        D::RefundDisputeHeld(d) => ("RefundDisputeHeld", d),
        D::DualControlRequired(d) => ("DualControlRequired", d),
        D::SelfApprovalForbidden(d) => ("SelfApprovalForbidden", d),
        D::ApprovalNotActionable(d) => ("ApprovalNotActionable", d),
        D::DualControlPolicyOutOfRange(d) => ("DualControlPolicyOutOfRange", d),
        D::TamperVerificationFailed(d) => ("TamperVerificationFailed", d),
        D::PolicyVersionViolation(d) => ("PolicyVersionViolation", d),
        D::TenantPostingLocked(d) => ("TenantPostingLocked", d),
        D::PeriodNotFound(d) => ("PeriodNotFound", d),
        D::ApprovalNotFound(d) => ("ApprovalNotFound", d),
        D::PayerPiiNotFound(d) => ("PayerPiiNotFound", d),
        // Guarded in the credit/debit-note handlers BEFORE the post, so it never
        // actually rides the sentinel — listed for the exhaustive match contract.
        D::NoteInvoiceNotFound(d) => ("NoteInvoiceNotFound", d),
        // Likewise guarded in the refund handler BEFORE the post (the origin
        // settlement is resolved out-of-txn); listed for the exhaustive contract.
        D::RefundOriginNotFound(d) => ("RefundOriginNotFound", d),
        D::Internal(d) => ("Internal", d),
    }
}

/// Reconstruct a [`DomainError`] from a sentinel tag + detail; an unrecognised
/// tag degrades to [`DomainError::Internal`] (never silently dropped).
fn domain_from_parts(tag: &str, detail: String) -> DomainError {
    use DomainError as D;
    match tag {
        "Unbalanced" => D::Unbalanced(detail),
        "Empty" => D::Empty(detail),
        "MixedPayer" => D::MixedPayer(detail),
        "MissingPayer" => D::MissingPayer(detail),
        "MixedLegalEntity" => D::MixedLegalEntity(detail),
        "InconsistentScale" => D::InconsistentScale(detail),
        "AmountOutOfRange" => D::AmountOutOfRange(detail),
        "EntryTooLarge" => D::EntryTooLarge(detail),
        "InvalidRequest" => D::InvalidRequest(detail),
        "ScaleOutOfRange" => D::ScaleOutOfRange(detail),
        "CreditResidualUndisposed" => D::CreditResidualUndisposed(detail),
        "MoneyOutCapExceeded" => D::MoneyOutCapExceeded(detail),
        "AllocationTooLarge" => D::AllocationTooLarge(detail),
        "AllocationCurrencyMismatch" => D::AllocationCurrencyMismatch(detail),
        "CurrencyMismatch" => D::CurrencyMismatch(detail),
        "FxRateUnavailable" => D::FxRateUnavailable(detail),
        "FxRateStaleNotAllowed" => D::FxRateStaleNotAllowed(detail),
        "AllocationSplitInvalid" => D::AllocationSplitInvalid(detail),
        "GrantExceedsUnallocated" => D::GrantExceedsUnallocated(detail),
        "CreditExceedsOpenAr" => D::CreditExceedsOpenAr(detail),
        "CreditExceedsWallet" => D::CreditExceedsWallet(detail),
        "ScheduleTooLong" => D::ScheduleTooLong(detail),
        "SspSnapshotRequired" => D::SspSnapshotRequired(detail),
        "MissingPoAllocationGroup" => D::MissingPoAllocationGroup(detail),
        "RecognitionPolicyConflict" => D::RecognitionPolicyConflict(detail),
        "CreditNoteSplitAmbiguous" => D::CreditNoteSplitAmbiguous(detail),
        "CreditNoteExceedsHeadroom" => D::CreditNoteExceedsHeadroom(detail),
        "RefundExceedsSettled" => D::RefundExceedsSettled(detail),
        "RefundExceedsAllocated" => D::RefundExceedsAllocated(detail),
        "ModificationTreatmentReview" => D::ModificationTreatmentReview(detail),
        "RecognitionWithoutInvoiceLink" => D::RecognitionWithoutInvoiceLink(detail),
        "PiiInMetadataValue" => D::PiiInMetadataValue(detail),
        "MissingInvestigationReason" => D::MissingInvestigationReason(detail),
        "CrossTenantAccessDenied" => D::CrossTenantAccessDenied(detail),
        "PeriodClosed" => D::PeriodClosed(detail),
        "AccountClosed" => D::AccountClosed(detail),
        "PayerClosed" => D::PayerClosed(detail),
        "AccountMappingMissing" => D::AccountMappingMissing(detail),
        "NegativeBalance" => D::NegativeBalance(detail),
        "SettlementReturnOverAllocated" => D::SettlementReturnOverAllocated(detail),
        "InvalidDisputeTransition" => D::InvalidDisputeTransition(detail),
        "ChargebackExceedsSettled" => D::ChargebackExceedsSettled(detail),
        "ChargebackOnRefunded" => D::ChargebackOnRefunded(detail),
        "ClockSkewQuarantine" => D::ClockSkewQuarantine(detail),
        "PeriodNotOpen" => D::PeriodNotOpen(detail),
        "PeriodCloseBlocked" => D::PeriodCloseBlocked(detail),
        "PeriodCloseInProgress" => D::PeriodCloseInProgress(detail),
        "IdempotencyConflict" => D::IdempotencyConflict(detail),
        "CurrencyScaleLocked" => D::CurrencyScaleLocked(detail),
        "OverRecognition" => D::OverRecognition(detail),
        "RefundClawbackDeferred" => D::RefundClawbackDeferred(detail),
        "FxOperationUnsupported" => D::FxOperationUnsupported(detail),
        "RefundDisputeHeld" => D::RefundDisputeHeld(detail),
        "DualControlRequired" => D::DualControlRequired(detail),
        "SelfApprovalForbidden" => D::SelfApprovalForbidden(detail),
        "ApprovalNotActionable" => D::ApprovalNotActionable(detail),
        "DualControlPolicyOutOfRange" => D::DualControlPolicyOutOfRange(detail),
        "TamperVerificationFailed" => D::TamperVerificationFailed(detail),
        "PolicyVersionViolation" => D::PolicyVersionViolation(detail),
        "TenantPostingLocked" => D::TenantPostingLocked(detail),
        "PeriodNotFound" => D::PeriodNotFound(detail),
        "ApprovalNotFound" => D::ApprovalNotFound(detail),
        "PayerPiiNotFound" => D::PayerPiiNotFound(detail),
        "NoteInvoiceNotFound" => D::NoteInvoiceNotFound(detail),
        "RefundOriginNotFound" => D::RefundOriginNotFound(detail),
        "ManualAdjustmentNotAllowed" => D::ManualAdjustmentNotAllowed(detail),
        _ => D::Internal(detail),
    }
}

#[cfg(test)]
mod tests {
    use super::{AlarmSeverity, DomainError, alarm_for};
    use crate::infra::events::alarm_catalog;

    /// The emitter MUST take its severity from the normative §4.7 catalog. An
    /// idempotency-key collision is Critical per AC #19 — the regression guard
    /// for the prior hardcoded `Warn`.
    #[test]
    fn idempotency_conflict_alarm_is_catalog_critical() {
        let (category, severity, code) =
            alarm_for(&DomainError::IdempotencyConflict("dup".to_owned()))
                .expect("an idempotency conflict raises an alarm");
        assert_eq!(code, "IDEMPOTENCY_PAYLOAD_CONFLICT");
        assert_eq!(
            severity.as_str(),
            AlarmSeverity::Critical.as_str(),
            "an idempotency-key collision is Critical (AC #19), not Warn"
        );
        assert_eq!(
            severity.as_str(),
            alarm_catalog::severity(category).as_str(),
            "the emitter severity must equal the catalog severity"
        );
    }

    /// Every variant `alarm_for` maps MUST carry the catalog's severity — pins
    /// the no-drift property generally, not just for idempotency.
    #[test]
    fn emitter_severity_always_matches_catalog() {
        for err in [
            DomainError::IdempotencyConflict("x".to_owned()),
            DomainError::NegativeBalance("x".to_owned()),
        ] {
            let (category, severity, _) = alarm_for(&err).expect("variant raises an alarm");
            assert_eq!(
                severity.as_str(),
                alarm_catalog::severity(category).as_str()
            );
        }
    }
}
