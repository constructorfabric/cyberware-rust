//! The at-most-once gate over `pricing_idempotency_dedup`, and the source a
//! replay is answered from (`design/01-foundation.md` §3.7, §4.2 step 1,
//! `cpt-cf-bss-pricing-fr-mutation-idempotency`).
//!
//! **The gate is the insert, not a lookup.** [`claim`] writes the claim row with
//! an `INSERT... ON CONFLICT DO NOTHING` and reads back only when the conflict
//! actually fired. A read-then-write would admit *both* of two concurrent
//! requests carrying the same client key, which is the single situation a client
//! idempotency key exists for: between the read and the write nothing holds the
//! key, so both callers see it free. The conflict is recognized as the driver's
//! typed `DbErr::RecordNotInserted` rather than by matching an error message —
//! message matching makes the gate a string comparison against a backend's
//! wording, and it fails open the first time that wording changes.
//!
//! **`claim` and `record_response` MUST run inside the same transaction as the
//! mutation they guard.** That is why both take a transaction handle instead of
//! a connection, and it is the contract everything below rests on. Under it a
//! losing duplicate's insert blocks on the winner's uncommitted row and, once
//! released, either sees the committed response and replays it, or finds nothing
//! left to conflict with — the winner having rolled back — and claims the key
//! itself. Split across two transactions the guarantee inverts: the claim
//! commits while the mutation does not, and every retry of a request that never
//! happened is answered "already done", forever.
//!
//! **A mismatching hash is refused before the stored response is even looked
//! at.** The same key arriving with a different payload is a mismatch whether or
//! not the first request has finished, so
//! [`RepoError::IdempotencyPayloadMismatch`] does not wait on the answer: there
//! is no state of the first request that makes replaying somebody else's
//! response to this one correct. Such a request is never replayed and never
//! re-executed.
//!
//! **Expiry is evaluated at claim time, which is what makes the TTL real.** A
//! conflicting row older than the configured window is taken over rather than
//! honoured — the key was forgotten when it expired, so nothing about it can be
//! replayed or contradicted, and a takeover is simply the fresh claim it is
//! entitled to. Doing it here rather than in a sweeper means the window holds on
//! the very first request past it, with no reaper running, no reaper lag, and no
//! silent extension of the window when a reaper is down. Reclaiming the storage
//! an expired row occupies is a separate concern and is not implemented here.
//!
//! **A duplicate meeting an unanswered claim is a refusal, and it has a code.**
//! [`RepoError::IdempotencyKeyInFlight`] answers it, because the only
//! alternative is to hand a caller a response nobody ever produced. Two paths
//! reach it, and they are not equally likely:
//!
//! - *Meeting an unanswered fresh claim.* Unreachable under the one-transaction
//!   contract: the transaction holding that claim has not committed, so the
//!   duplicate's insert is still blocked on it and, when released, sees either
//!   the committed response or nothing left to conflict with. Reaching it here
//!   means the contract was violated, and refusing is how that violation
//!   becomes visible instead of becoming a fabricated reply.
//! - *Losing a takeover race.* **Reachable in production, with no contract
//!   violation by anyone.** Nothing holds an expired row between a transaction's
//!   conflict check and its takeover UPDATE, so two duplicates arriving on the
//!   same expired key can both clear the conflict check and both read the same
//!   expired row. The compare-and-swap in [`take_over`] decides; the loser is
//!   refused. Note that the loser may be carrying a **different** payload from
//!   the winner — on this one path a mismatching request is refused in-flight
//!   rather than for the mismatch. At-most-once is intact either way, because
//!   the loser does not execute.
//!
//! D-143 names the case — `IDEMPOTENCY_KEY_IN_FLIGHT` (409), *this key is claimed
//! and not yet answered; retry, and you will receive either the stored response or
//! `IDEMPOTENCY_PAYLOAD_MISMATCH`* — so the refusal is an answer rather than an
//! outcome the surface has to decide about, and it is shaped like the mismatch
//! beside it: an `Err`, because the guarded mutation must not run and the caller has
//! something to do about it. The gap it closes is **live** and no tightening of the
//! transaction contract closes it, the second path being what keeps it open.
//!
//! [`take_over`]: take_over
//!
//! [`claim`]: IdempotencyGate::claim
//! [`record_response`]: IdempotencyGate::record_response

use std::time::Duration;

use aws_lc_rs::digest::{SHA256, digest as sha256};
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{ColumnTrait, Condition, DbErr, EntityTrait};
use serde_json::Value as JsonValue;
use time::OffsetDateTime;
use toolkit_db::secure::{
    AccessScope, DbTx, ScopeError, SecureEntityExt, SecureInsertExt, SecureUpdateExt,
};
use uuid::Uuid;

use crate::infra::storage::RepoError;
use crate::infra::storage::entity::idempotency_dedup;

/// The noun the refusals from this module name.
const SUBJECT: &str = "idempotency claim";

/// What a [`IdempotencyGate::claim`] found the key in.
///
/// Exactly the two states in which the caller has somewhere to go: it holds the
/// key and performs the mutation, or the key is answered and the answer is
/// handed back. Every other state of a duplicate is a refusal
/// ([`RepoError::IdempotencyPayloadMismatch`],
/// [`RepoError::IdempotencyKeyInFlight`]), which is what keeps "the guarded
/// mutation must not run" from being a condition a caller can forget to match
/// on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// The key is now held by this caller, which must go on to perform the
    /// guarded mutation and then record its response.
    Claimed,
    /// The key was already held by an identical request that finished, and this
    /// is the answer it was given. The guarded mutation must **not** run again.
    Replay {
        /// The status the original caller was told.
        status: i32,
        /// The body the original caller was told.
        body: JsonValue,
    },
}

/// The at-most-once gate over `pricing_idempotency_dedup`.
///
/// Holds the retention window (`config.limits.idempotency_key_ttl_hours`)
/// because expiry is decided on the claim path — see the module doc.
#[derive(Clone, Debug)]
pub struct IdempotencyGate {
    /// How long a claim keeps its key. Past it the key is forgotten and the next
    /// request carrying it claims afresh.
    ttl: time::Duration,
}

impl IdempotencyGate {
    /// Build over the configured idempotency-key retention window.
    ///
    /// A window too large for `chrono` to represent is kept as the largest one
    /// it can, which reads as "these keys never expire". The constructor stays
    /// infallible on purpose: the only caller is boot, the config validator
    /// already refuses a zero window, and saturating errs towards holding a
    /// claim rather than towards releasing one.
    #[must_use]
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl: time::Duration::try_from(ttl).unwrap_or(time::Duration::MAX),
        }
    }

    /// Claim `(tenant_id, operation, client_key)` inside `txn` for a request
    /// whose canonical payload digests to `request_hash`.
    ///
    /// The claim row is written with **no response**: the guarded operation has
    /// not run yet, and the gate is the insert itself rather than anything the
    /// row says. `now` is the authoring instant the caller is working from, used
    /// both as the claim's timestamp and as the reference the expiry window is
    /// measured against, so a request is never judged against a clock other than
    /// its own.
    ///
    /// # Errors
    /// [`RepoError::IdempotencyPayloadMismatch`] when the key is held by a
    /// request that is not this one — never replayed, never re-executed.
    /// [`RepoError::IdempotencyKeyInFlight`] when the key is held by a claim
    /// nobody has answered yet, so there is neither a response to replay nor a
    /// free key to take; the caller retries and is then replayed or refused for
    /// the mismatch (D-143). [`RepoError::Db`] on a scope or storage failure,
    /// which includes the conflicting row being unreadable inside the
    /// transaction that just collided with it. [`RepoError::CorruptRow`] when
    /// the held row carries half an answer, which the table's pairing `CHECK`
    /// forbids.
    #[allow(
        clippy::too_many_arguments,
        reason = "the composite key is three columns and the claim needs the \
                  digest, the instant and the caller's scope beside them; \
                  bundling them into a parameter struct would hide the key's \
                  own shape behind a type nothing else uses"
    )]
    pub async fn claim(
        &self,
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant_id: Uuid,
        operation: &str,
        client_key: &str,
        request_hash: &[u8],
        now: OffsetDateTime,
    ) -> Result<ClaimOutcome, RepoError> {
        let am = idempotency_dedup::ActiveModel {
            tenant_id: Set(tenant_id),
            operation: Set(operation.to_owned()),
            client_key: Set(client_key.to_owned()),
            request_hash: Set(request_hash.to_vec()),
            response_status: Set(None),
            response_body: Set(None),
            created_at_utc: Set(now),
        };
        let on_conflict = OnConflict::columns([
            idempotency_dedup::Column::TenantId,
            idempotency_dedup::Column::Operation,
            idempotency_dedup::Column::ClientKey,
        ])
        .do_nothing()
        .to_owned();

        match idempotency_dedup::Entity::insert(am.clone())
            .secure()
            .scope_with_model(scope, &am)
            .map_err(|e| RepoError::Db(format!("idempotency claim scope: {e}")))?
            .on_conflict_raw(on_conflict)
            .exec(txn)
            .await
        {
            Ok(_) => return Ok(ClaimOutcome::Claimed),
            // The key was already held; the conflict swallowed the insert.
            Err(ScopeError::Db(DbErr::RecordNotInserted)) => {}
            Err(e) => return Err(RepoError::Db(format!("idempotency claim: {e}"))),
        }

        // The insert was validated against the same scope, so a conflict this read
        // cannot see is not a foreign tenant's row — it is the store contradicting
        // itself inside one transaction.
        let held = read_held(txn, scope, tenant_id, operation, client_key)
            .await?
            .ok_or_else(|| {
                RepoError::Db(format!(
                    "idempotency claim {} conflicted but is not readable in the same transaction",
                    claim_ref(operation, client_key)
                ))
            })?;

        // Expiry is asked first, and the order matters: an expired row is not a
        // claim at all, so there is nothing for the arriving request to
        // contradict. Asking about the hash first would let one payload poison
        // its key past the TTL and — with no reaper in the gear — forever.
        if (now - held.created_at_utc) > self.ttl {
            return take_over(txn, scope, &held, request_hash, now).await;
        }

        if held.request_hash != request_hash {
            return Err(RepoError::IdempotencyPayloadMismatch {
                operation: operation.to_owned(),
                client_key: client_key.to_owned(),
            });
        }

        match (held.response_status, held.response_body) {
            (Some(status), Some(body)) => Ok(ClaimOutcome::Replay { status, body }),
            (None, None) => Err(RepoError::IdempotencyKeyInFlight {
                operation: operation.to_owned(),
                client_key: client_key.to_owned(),
            }),
            // `chk_pricing_idempotency_dedup_answered` makes half an answer
            // impossible, so a row holding one means something reached the
            // table around this gear rather than through it.
            (Some(_), None) | (None, Some(_)) => Err(RepoError::CorruptRow(format!(
                "pricing_idempotency_dedup {} holds half a response",
                claim_ref(operation, client_key)
            ))),
        }
    }

    /// The answer already recorded under `client_key`, if there is one.
    ///
    /// A **read**, and deliberately not a claim: it opens nothing, so a caller may
    /// ask before deciding which act it is performing. That is what it exists for.
    /// `move_membership` forks on the clock — `effective_from <= now()` takes an immediate arm
    /// that opens no idempotency claim at all — so a retry under the same key arriving after
    /// that instant has passed would take a different arm from the call it is retrying, never
    /// consult the record, and open an approval unit for a move that already committed. Asking
    /// here means the fork cannot outrun the guard.
    ///
    /// `None` covers every state that is not a completed answer: no row, an expired
    /// one, a claim still in flight. Each of those is a request that should proceed,
    /// and the claim inside [`Self::guarded`] is what adjudicates it properly — this
    /// read exists to catch the one state the fork above it would otherwise miss.
    ///
    /// A payload mismatch is **not** swallowed: a caller reusing a key for different
    /// content is refused here exactly as [`Self::claim`] would refuse it, because
    /// answering the first request's body to a second, different request is the one
    /// outcome worse than either arm.
    ///
    /// # Errors
    /// [`RepoError::IdempotencyPayloadMismatch`] when the key is held for a
    /// different payload; [`RepoError::Db`] on a scope or storage failure —
    /// including a read that could not run, which is **not** answered as an absent
    /// claim.
    #[allow(
        clippy::too_many_arguments,
        reason = "the read takes the same operands as [`Self::claim`] because it \
                  answers about the same row: the three key columns, the digest \
                  it must agree with, the instant expiry is measured against and \
                  the caller's scope"
    )]
    pub async fn recorded_response(
        &self,
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant_id: Uuid,
        operation: &str,
        client_key: &str,
        request_hash: &[u8],
        now: OffsetDateTime,
    ) -> Result<Option<(i32, JsonValue)>, RepoError> {
        // **Only the absent row is `Ok(None)`.** A storage fault propagates: this
        // read's one caller asks it in order to decide whether the act it is about
        // to perform has already been performed, so answering "nothing recorded"
        // to a read that could not run produces exactly the duplicate the guard
        // exists to refuse.
        let Some(held) = read_held(txn, scope, tenant_id, operation, client_key).await? else {
            return Ok(None);
        };
        if (now - held.created_at_utc) > self.ttl {
            return Ok(None);
        }
        if held.request_hash != request_hash {
            return Err(RepoError::IdempotencyPayloadMismatch {
                operation: operation.to_owned(),
                client_key: client_key.to_owned(),
            });
        }
        match (held.response_status, held.response_body) {
            (Some(status), Some(body)) => Ok(Some((status, body))),
            // The claim is in flight: written, not yet answered.
            (None, None) => Ok(None),
            // `claim`'s arm on the identical shape, and for its reason:
            // `chk_pricing_idempotency_dedup_answered` makes half an answer
            // impossible, so a row holding one reached the table around this gear.
            // Folding it to `None` here told the one caller "nothing recorded" and
            // let it perform the act a second time — the duplicate the paragraph
            // above says this read exists to refuse, on the strength of a row
            // nobody can read.
            //
            // Unreachable while the CHECK stands on both backends, so no test can
            // reach it; the argument is that the two readers of this column must
            // not disagree about what it means.
            (Some(_), None) | (None, Some(_)) => Err(RepoError::CorruptRow(format!(
                "pricing_idempotency_dedup {} holds half a response",
                claim_ref(operation, client_key)
            ))),
        }
    }

    /// Record what the guarded operation answered, into the row this caller
    /// claimed.
    ///
    /// The last statement of the guarded transaction, and the one that turns a
    /// claim into something replayable. Associated rather than a method: the
    /// retention window has no bearing on writing an answer down.
    ///
    /// **Write-once, enforced by the statement.** The predicate carries
    /// `response_status IS NULL` beside the key, so an answer can be recorded
    /// only into a claim that has none. Leaving it off would make this the one
    /// write in the module that a caller's discipline had to get right: a second
    /// `record_response` — a retry, a handler that answered twice, a caller that
    /// recorded after being told `Replay` — would overwrite the answer an
    /// earlier caller was already given, and every later replay would then serve
    /// a response that contradicts what the original request was told. The whole
    /// point of the row is that the stored answer is the one that was actually
    /// sent.
    ///
    /// The cost is that [`RepoError::NotFound`] now covers two situations: no
    /// visible claim, and a claim that is already answered. That conflation is
    /// accepted rather than resolved with the extra read
    /// [`PlanRepo::refuse`](super::PlanRepo) takes, because unlike a failed
    /// compare-and-swap the two cases call for the same response — neither is
    /// retryable, and in both the caller does not hold an unanswered claim,
    /// which is the only state in which recording an answer means anything.
    ///
    /// # Errors
    /// [`RepoError::NotFound`] when `scope` holds no **unanswered** claim for the
    /// key — inside the transaction that claimed it, that means the response is
    /// being recorded against a claim this caller does not hold, or a second
    /// time. [`RepoError::Db`] on a scope or storage failure, including the
    /// table's own refusal of a status outside 100-599; that range is left to
    /// the `CHECK` rather than pre-checked here, because the surface owns which
    /// statuses it may answer with and a second opinion here would only be able
    /// to disagree with it.
    pub async fn record_response(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant_id: Uuid,
        operation: &str,
        client_key: &str,
        status: i32,
        body: JsonValue,
    ) -> Result<(), RepoError> {
        let result = idempotency_dedup::Entity::update_many()
            .secure()
            .scope_with(scope)
            .col_expr(
                idempotency_dedup::Column::ResponseStatus,
                Expr::value(Some(status)),
            )
            .col_expr(
                idempotency_dedup::Column::ResponseBody,
                Expr::value(Some(body)),
            )
            .filter(
                key_of(tenant_id, operation, client_key)
                    .add(idempotency_dedup::Column::ResponseStatus.is_null()),
            )
            .exec(txn)
            .await
            .map_err(|e| RepoError::Db(format!("record idempotency response: {e}")))?;

        if result.rows_affected == 0 {
            return Err(RepoError::NotFound {
                subject: SUBJECT.to_owned(),
                id: claim_ref(operation, client_key),
            });
        }
        Ok(())
    }

    /// The FIPS-validated (`aws-lc-rs`) SHA-256 digest of a canonical request
    /// rendering — the same crypto provider the platform installs at bootstrap,
    /// so no non-FIPS hasher enters the graph.
    ///
    /// **Building `canonical` is the caller's job**, and deliberately so: only
    /// the surface receiving a request knows which of its fields are semantic
    /// and which are transport. A correlation id, a trace header or a retry
    /// counter folded into the digest would make every honest retry look like a
    /// different request and turn `IDEMPOTENCY_PAYLOAD_MISMATCH` into the normal
    /// answer to a retry. The caller owes a stable rendering: the same intent
    /// must produce the same string, byte for byte, on every attempt.
    ///
    /// **Only the digest is stored, never the payload.** The dedup table has no
    /// retention story of its own, and keeping request bodies in it would put a
    /// second, unmanaged copy of what callers sent beside the audit trail that
    /// is supposed to be the one place it lives.
    #[must_use]
    pub fn payload_hash(canonical: &str) -> Vec<u8> {
        sha256(&SHA256, canonical.as_bytes()).as_ref().to_vec()
    }
}

/// Take an expired claim over: the arriving request's digest, no response, and
/// a fresh timestamp, under a predicate matching the row **as it was read**.
///
/// That predicate is the whole of the race protection, and the race is a real
/// one that **no contract prevents**: nothing holds an expired row between a
/// transaction's conflict check and this UPDATE, so two duplicates can both find
/// the same row expired and both arrive here carrying the same `held`. Without
/// the predicate both would rewrite the row, both would be told they claimed it,
/// and the guarded mutation would run twice under one key — precisely the
/// outcome the table exists to prevent. With it exactly one UPDATE matches: the
/// second blocks on the first, and on release re-evaluates against the row the
/// winner already moved, which no longer satisfies `created_at_utc = <the old
/// value>`.
///
/// The loser is refused [`RepoError::IdempotencyKeyInFlight`], which is the
/// honest reading of where it stands — another caller now holds a claim nobody
/// has answered. It is **not** told the mismatch refusal even when its payload
/// differs from the winner's: this transaction never compared the two, and
/// inventing a comparison here would refuse a caller on the strength of a digest
/// it never lost a race to. At-most-once is intact either way, since the loser
/// executes nothing.
///
/// # Errors
/// [`RepoError::IdempotencyKeyInFlight`] for the loser of the swap;
/// [`RepoError::Db`] on a scope or storage failure.
async fn take_over(
    txn: &DbTx<'_>,
    scope: &AccessScope,
    held: &idempotency_dedup::Model,
    request_hash: &[u8],
    now: OffsetDateTime,
) -> Result<ClaimOutcome, RepoError> {
    let result = idempotency_dedup::Entity::update_many()
        .secure()
        .scope_with(scope)
        .col_expr(
            idempotency_dedup::Column::RequestHash,
            Expr::value(request_hash.to_vec()),
        )
        .col_expr(
            idempotency_dedup::Column::ResponseStatus,
            Expr::value(None::<i32>),
        )
        .col_expr(
            idempotency_dedup::Column::ResponseBody,
            Expr::value(None::<JsonValue>),
        )
        .col_expr(idempotency_dedup::Column::CreatedAtUtc, Expr::value(now))
        .filter(
            key_of(held.tenant_id, &held.operation, &held.client_key)
                .add(idempotency_dedup::Column::CreatedAtUtc.eq(held.created_at_utc)),
        )
        .exec(txn)
        .await
        .map_err(|e| RepoError::Db(format!("take over expired idempotency claim: {e}")))?;

    if result.rows_affected == 0 {
        return Err(RepoError::IdempotencyKeyInFlight {
            operation: held.operation.clone(),
            client_key: held.client_key.clone(),
        });
    }
    Ok(ClaimOutcome::Claimed)
}

/// Read the claim row under one key, scoped.
///
/// **`Ok(None)` is the absent row and nothing else.** The two callers want
/// opposite things from it — [`Self::claim`] has just collided with the row, so an
/// absent one is the store contradicting itself and it renders that as a storage
/// failure; [`Self::recorded_response`] is asking whether a claim exists at all,
/// and an absent one is its ordinary answer. Folding the two here, by returning
/// `Err` for the absent row, made the caller that has to distinguish them unable
/// to: it read every error as "no claim", which is the answer that says *go ahead
/// and perform the act*.
async fn read_held(
    txn: &DbTx<'_>,
    scope: &AccessScope,
    tenant_id: Uuid,
    operation: &str,
    client_key: &str,
) -> Result<Option<idempotency_dedup::Model>, RepoError> {
    idempotency_dedup::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(key_of(tenant_id, operation, client_key))
        .one(txn)
        .await
        .map_err(|e| RepoError::Db(format!("read held idempotency claim: {e}")))
}

/// The composite primary key, as a filter. One spelling, so no statement here
/// can address a row by fewer axes than the key actually has.
fn key_of(tenant_id: Uuid, operation: &str, client_key: &str) -> Condition {
    Condition::all()
        .add(idempotency_dedup::Column::TenantId.eq(tenant_id))
        .add(idempotency_dedup::Column::Operation.eq(operation))
        .add(idempotency_dedup::Column::ClientKey.eq(client_key))
}

/// The key as one reference a refusal can carry. The tenant is left out: it is
/// already the scope every refusal is raised within, and a tenant id in a
/// message reaching another tenant would say more than the refusal means to.
fn claim_ref(operation: &str, client_key: &str) -> String {
    format!("{operation}/{client_key}")
}

#[cfg(test)]
#[path = "idempotency_repo_tests.rs"]
mod idempotency_repo_tests;
