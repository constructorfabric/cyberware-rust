//! The one transaction the at-most-once contract requires: claim, mutate,
//! render, record, commit.
//!
//! `idempotency_repo`'s module doc states the contract in as many words and this
//! module is what makes it satisfiable: [`IdempotencyGate::claim`] and
//! [`IdempotencyGate::record_response`] **MUST** run inside the same transaction
//! as the mutation they guard, because "split across two transactions the
//! guarantee inverts: the claim commits while the mutation does not, and every
//! retry of a request that never happened is answered 'already done', forever."
//!
//! # Why the seam is here and not in the handler or the repository
//!
//! Neither of the other two candidates can hold that transaction.
//!
//! - `PlanRepo::create_draft` runs through the repository's **private**
//!   connection, which a caller cannot borrow and cannot make join a transaction
//!   it opened. `PriceRepo::create_draft` opens its **own**, and the toolkit
//!   refuses `Db::conn()` inside an open transaction, so it cannot be nested in
//!   one either. Both now have runner-taking forms —
//!   [`plan_repo::create_draft_on`] and [`price_repo::create_draft_on`] — which
//!   are *extracted* from those methods rather than copied, so the guarded path
//!   and the direct one issue the same statements.
//! - A handler must not become the transaction owner regardless. `api/rest` is
//!   where DTOs and the authz gate live; a handler holding a `DbTx` would put
//!   rollback semantics and the commit boundary into the transport layer, and
//!   two handlers would spell one contract twice.
//!
//! So it sits beside [`crate::infra::publish`], which is the other thing in this
//! crate that owns a transaction and composes repositories inside it, and it is
//! the answer to that module's "Idempotency is deliberately not wired here".
//!
//! [`plan_repo::create_draft_on`]: crate::infra::storage::repo::plan_repo::create_draft_on
//! [`price_repo::create_draft_on`]: crate::infra::storage::repo::price_repo::create_draft_on
//!
//! # Why the caller supplies the renderer
//!
//! **The recorded body must be the body that was actually sent.** A replay hands
//! back a stored status and a stored body verbatim; if this module rendered
//! something of its own — the domain value, a summary, anything but the response
//! — a replay would answer the second caller something the first caller never
//! received, which defeats the only reason the response is stored at all.
//!
//! The renderer is a **closure** rather than a type parameter naming a view,
//! because DE0202 forbids `infra` from importing a `*Dto` / `*Response` /
//! `*Query` type and the response types live in `api::rest` by DE0201. A closure
//! carries no type name across that boundary, so the body is produced by the
//! layer that owns the wire shape and this module never learns what it is.
//!
//! # What a caller can be told
//!
//! This is the first path in the gear that can put `IDEMPOTENCY_KEY_IN_FLIGHT`
//! on the wire. D-143 minted that code *because* "the surface layer had nothing
//! to answer a client with"; the outcome existed and the answer did not. Both
//! that refusal and `IDEMPOTENCY_PAYLOAD_MISMATCH` come back through the single
//! [`repo_failure`] ladder as the 409s they already map to — this module invents
//! no status and no code.

use std::future::Future;
use std::pin::Pin;

use serde_json::Value as JsonValue;
use toolkit_db::secure::{AccessScope, DbTx, TxError};
use toolkit_db::{DBProvider, DbError};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::infra::storage::repo::{ClaimOutcome, IdempotencyGate};
use crate::infra::storage::repo_failure;
use time::OffsetDateTime;

/// What a guarded mutation returns when it runs inside the caller's transaction.
///
/// **The error is a [`DomainError`], not a [`RepoError`].** The narrower type is
/// honest for a caller handing this a single repository call, and wrong for a
/// window mutation — a whole domain pipeline whose refusals (`WINDOW_GAP`,
/// `WINDOW_OVERLAP`, a materiality refusal) have no `RepoError` to be spelled as.
/// Each caller maps what *it* produces, through the same [`repo_failure`] ladder,
/// so nothing about which refusal reaches the wire depends on this choice.
///
/// A `Pin<Box<dyn Future>>` rather than an `impl Future` because
/// `Db::in_transaction` takes a higher-ranked closure over the transaction
/// handle's lifetime, and a boxed future is the shape that bound accepts —
/// `infra::publish` writes the same thing at its own call site.
pub type TxFuture<'t, T> = Pin<Box<dyn Future<Output = Result<T, DomainError>> + Send + 't>>;

/// The outcome of a guarded request.
///
/// Two arms because the caller genuinely has to answer differently: a performed
/// mutation gets its own response (and may set headers the replay cannot
/// reconstruct, such as `Location` and `ETag`), while a replay must reproduce
/// the **stored** status and body and must not re-derive either.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Guarded<T> {
    /// The key was free, the mutation ran under this caller's claim, and its
    /// response was recorded.
    Performed(T),
    /// The key was already answered by an identical request. **Nothing ran.**
    Replayed {
        /// The status the original caller was told.
        status: i32,
        /// The body the original caller was told, verbatim.
        body: JsonValue,
    },
}

/// Everything about a guarded request that is not the mutation itself.
///
/// A parameter object rather than seven arguments: `operation` and `client_key`
/// are two thirds of a composite primary key, and passing them positionally
/// beside a tenant and a digest is how two of them end up swapped.
#[derive(Clone, Debug)]
pub struct GuardedRequest {
    /// The operation the key is scoped to — a per-route constant, so one client
    /// key used on two different verbs does not collide.
    pub operation: &'static str,
    /// The client-supplied key.
    pub client_key: String,
    /// The digest of the canonical request rendering.
    pub request_hash: Vec<u8>,
    /// The tenant the row is written to.
    pub tenant_id: Uuid,
    /// The status a **performed** mutation answers with, recorded so a replay
    /// answers the same one.
    ///
    /// **Nothing binds this to the status the handler's `Performed` arm sends**,
    /// and the two are separate literals at every call site. A route whose
    /// success status is changed on one arm only — or one whose `Performed` arm
    /// branches between 200 and 201 — answers a replay a status the first caller
    /// never received, silently: the dedup row is the only record of what that
    /// caller was told. Binding them means the guard, not the handler, deciding
    /// the response status on both arms.
    pub status: i32,
    /// The authoring instant, used as the claim's timestamp and as the reference
    /// its expiry window is measured against — so a request is never judged
    /// against a clock other than its own.
    pub now: OffsetDateTime,
}

/// Run `mutation` at most once for `request.client_key`, inside one transaction
/// with the claim that guards it.
///
/// On [`ClaimOutcome::Replay`] the mutation is **not invoked** and the stored
/// status and body come back untouched. On [`ClaimOutcome::Claimed`] the
/// mutation runs against the same transaction handle, `render` turns its result
/// into the response body, that body is recorded, and the whole thing commits
/// together. A failure anywhere rolls the claim back with the mutation, which is
/// the property the module doc's central argument predicts: a retry of a key
/// whose request never happened claims afresh rather than being told "already
/// done" forever.
///
/// # Errors
/// [`DomainError::IdempotencyPayloadMismatch`] (409) when the key is held by a
/// different request; [`DomainError::IdempotencyKeyInFlight`] (409) when it is
/// held by a claim nobody has answered (D-143); whatever the mutation itself
/// refuses, through the single [`repo_failure`] ladder; whatever `render`
/// refuses; and [`DomainError::Internal`] when the transaction cannot be begun
/// or committed.
pub async fn guarded<T, M, R>(
    db: &DBProvider<DbError>,
    gate: &IdempotencyGate,
    scope: &AccessScope,
    request: GuardedRequest,
    mutation: M,
    render: R,
) -> Result<Guarded<T>, DomainError>
where
    T: Send + 'static,
    M: for<'t> FnOnce(&'t DbTx<'t>) -> TxFuture<'t, T> + Send + 'static,
    R: FnOnce(&T) -> Result<JsonValue, DomainError> + Send + 'static,
{
    let gate = gate.clone();
    let scope = scope.clone();
    let (_, outcome) = db
        .db()
        .in_transaction::<Guarded<T>, DomainError, _>(move |txn| {
            Box::pin(async move {
                let claimed = gate
                    .claim(
                        txn,
                        &scope,
                        request.tenant_id,
                        request.operation,
                        &request.client_key,
                        &request.request_hash,
                        request.now,
                    )
                    .await
                    .map_err(|e| repo_failure(&e))?;

                // The early return is the whole of at-most-once: a replay must
                // reach neither the mutation nor `record_response`, whose
                // write-once predicate would refuse the second answer anyway.
                match claimed {
                    ClaimOutcome::Replay { status, body } => {
                        return Ok(Guarded::Replayed { status, body });
                    }
                    ClaimOutcome::Claimed => {}
                }

                let performed = mutation(txn).await?;
                let body = render(&performed)?;
                IdempotencyGate::record_response(
                    txn,
                    &scope,
                    request.tenant_id,
                    request.operation,
                    &request.client_key,
                    request.status,
                    body,
                )
                .await
                .map_err(|e| repo_failure(&e))?;
                Ok(Guarded::Performed(performed))
            })
        })
        .await;
    outcome.map_err(tx_failure)
}

/// Flatten the transaction wrapper back into the gear's rejection vocabulary.
///
/// The body's error type is [`DomainError`] precisely so a typed refusal
/// survives the rollback: a guarded create that met a duplicate scope key has to
/// reach the caller as `DUPLICATE_SCOPE_KEY`, not as "the store failed". What
/// arrives as infrastructure is only what happens outside the body — beginning
/// the transaction, and committing it.
pub(crate) fn tx_failure(err: TxError<DomainError>) -> DomainError {
    err.into_domain(|infra| DomainError::Internal(format!("guarded mutation transaction: {infra}")))
}

#[cfg(test)]
#[path = "idempotent_tests.rs"]
mod idempotent_tests;
