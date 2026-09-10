//! The writer of `pricing_outbox` — the transactional enqueue, and the one
//! definition of the `PlanPublished` payload.
//!
//! Transactional is the whole point of the table: **an event exists if and only
//! if its commit happened**. So this module, like
//! [`audit_repo`](super::audit_repo), is stateless and takes a **runner** rather
//! than a provider. An enqueue in a transaction of its own could commit while
//! the publish rolled back — a `PlanPublished` for a plan that is still `draft`,
//! delivered at-least-once to every consumer — or fail while the publish
//! committed, which is a version nothing is ever told about.
//!
//! # Three things this file decides, each with its reason
//!
//! **The sequence is per aggregate and never global.** `seq` is
//! `MAX(seq) + 1` over `(tenant_id, aggregate_id)`, guarded by
//! `uq_pricing_outbox_sequence`. A global sequence would serialize every
//! tenant's publishing behind one counter, and no consumer needs cross-aggregate
//! order — `01-foundation.md` §1.2 orders the frozen event set per
//! `(tenantId, aggregateId)` and says nothing about a total order. The posture
//! is the audit chain's: the read of the head is not the guard, the unique index
//! is, and the loser's whole transaction rolls back.
//!
//! **The dedup key is derived here, never at the call site.** For a plan
//! publish the natural key is the publish unit itself — `(event name, plan_id,
//! revision)` — because a revision publishes exactly **once**: D-90 gives a
//! draft revision one edge out, and the publish commit's compare-and-swap
//! refuses the second attempt. Under `uq_pricing_outbox_dedup_key
//! (tenant_id, dedup_key)` a second `PlanPublished` for one revision therefore
//! fails at the **writer** rather than at every consumer, which is what that
//! index is for. Derived in [`plan_published_dedup_key`] so that no caller can
//! spell it a second way; a dedup key spelled twice is a dedup key that dedups
//! nothing.
//!
//! **`published_at` is NULL and stays NULL.** Draining is the relay's job and no
//! part of the publish commit; `idx_pricing_outbox_undrained` is that relay's
//! cursor. Note also that `config.events_enabled` (default `false`) gates
//! **fan-out, not the row** — [`crate::config`] says so in as many words — so
//! [`enqueue`] is unconditional and never reads the flag. A publish whose row
//! was skipped because fan-out was off would be a publish nothing could ever
//! replay.
//!
//! # The event name comes from one place
//!
//! `NewOutboxEvent` carries a [`CatalogEvent`] rather than a `String`, and the
//! column is written from [`CatalogEvent::as_str`].
//! `chk_pricing_outbox_event_name` pins the same set, so a second spelling
//! *would* be caught — but caught as a driver error inside a publish
//! transaction, which is not where a frozen contract should be discovered.
//! The count is deliberately not in that sentence, and it is not in the two below
//! either: a number here is the one thing in them that can go stale and the only
//! thing they do not need — the CHECK and the enum widen together, the prose does
//! not. `CatalogEvent::ALL` is the roster and `domain::events_tests` is where its
//! size is asserted.
//!
//! ## How to check the discipline holds — and the check that does **not** work
//!
//! Not *"`NewOutboxEvent {` appears only in this file"*. Two sites in
//! [`crate::infra::repricing`] are struct **updates** — `NewOutboxEvent {
//! dedup_key: …,..NewOutboxEvent::price_updated(…) }` — which *extend* a named
//! constructor rather than replacing it: the name, the aggregate and the payload
//! still come from the constructor and only the key moves, to the `(run_id,
//! price_id)` shape `12-operator-efficiency.md` requires of a repricing run. A
//! grep for the opening brace cannot tell that from a literal built from nothing,
//! and it has already reported those two as regressions they are not.
//!
//! What separates the two is the functional-update base, so **count it**:
//!
//! ```text
//! diff <(rg -c 'NewOutboxEvent \{'    src --glob '!**/outbox_repo*.rs') \
//!      <(rg -c '\.\.NewOutboxEvent::' src --glob '!**/outbox_repo*.rs')
//! ```
//!
//! Per file the two counts must be equal: every literal outside this module is a
//! struct update over a constructor, so a from-scratch literal breaks the
//! equality and a struct update preserves it. The other half of the discipline is
//! the **name**: no `CatalogEvent` variant may be spelled as a literal by a
//! *writer*, so
//!
//! ```text
//! rg -n 'format!\("[A-Za-z]*(Published|Updated|Created|Retired|Scheduled|Activated|Expired|Cancelled|Degraded)' \
//!    src --glob '!**/*_tests.rs'
//! ```
//!
//! must come back empty. The exclusion is deliberate and is not a loophole: a
//! test asserting a rendered key spells it independently **on purpose**, which is
//! the only thing that makes the assertion say anything.
//!
//! # A gap this file named, and what filled it
//!
//! [`PricingSnapshotRef`](crate::domain::snapshot::PricingSnapshotRef) has three
//! parts: the version ref, the resolved price ids, and an
//! `evaluation_policy_version`. Until D-162 the commit produced the first two
//! and this file stamped nothing for the third, because no document in this gear
//! or in Rating named a producer or a format for it and a placeholder in a
//! published payload is a value a consumer pins against — the first thing to pin
//! against an invented policy version being the last thing that could tell it
//! was invented (D-161).
//!
//! D-162 gives it a producer: the **evaluation-policy generation**, a declared
//! constant of this gear ([`EVALUATION_POLICY_GENERATION`]) naming which
//! evaluation-policy field set the frozen row content is to be read under. So
//! the payload now carries all three segments. The ban stands — what is stamped
//! is the generation the gear declares and whose roster
//! `01-foundation.md` §4.4 pins, never a filler.
//!
//! # One more naming decision, recorded because nothing else records it
//!
//! **No document declares the `PlanPublished` payload's field list.** The design
//! set fixes the event *name*, the ordering key, the at-least-once delivery and
//! the requirement that the event carry a pending version ref and a correlation
//! key — and nothing else. The wire keys below are therefore this module's, in
//! the `camelCase` the design set spells consumer-visible fields in
//! (`pricingSnapshotRef`, `planId`); they are written here once so a later
//! document has something concrete to contradict.

use bss_pricing_sdk::CatalogVersion;

use sea_orm::ActiveValue::Set;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order};
use serde_json::{Value as JsonValue, json};
use toolkit_db::secure::{AccessScope, DBRunner, SecureEntityExt, SecureInsertExt};
use uuid::Uuid;

use crate::domain::bundle::{InvoiceItemization, PriceBasis};
use crate::domain::evaluation_policy::EVALUATION_POLICY_GENERATION;
use crate::domain::events::CatalogEvent;
use crate::domain::instant::format_rfc3339;
use crate::domain::overlay::{ScopeSelector, ScopeValue};
use crate::domain::scope_key::PlanId;
use crate::infra::storage::entity::outbox;
use crate::infra::storage::{RepoError, contention_or_db};
use time::OffsetDateTime;

/// The `PlanPublished` payload, as one type rather than a map built at a call
/// site.
///
/// The version ref is the registry's **pending** handle and never a committed
/// version: §4.2 step 4 is explicit that `PlanPublished` carries a pending ref,
/// because the registry batches and the version does not exist yet. A consumer
/// that received a committed version here would be reading a number this gear
/// invented.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanPublishedPayload {
    /// The plan that published.
    pub plan_id: PlanId,
    /// The revision that became current.
    pub revision: u64,
    /// The registry's pending handle.
    pub pending_version_ref: String,
    /// The price rows this commit moved into `published`.
    pub price_ids: Vec<Uuid>,
    /// The correlation id of the causing request.
    pub correlation_id: Uuid,
}

impl PlanPublishedPayload {
    /// Render the payload for its `jsonb` column.
    ///
    /// Built with `json!` rather than a `Serialize` derive so the **wire** keys
    /// are spelled in exactly one place and are visibly a different vocabulary
    /// from the Rust field names — the sibling precedent is `price_repo`'s
    /// `allowance_json`, which persists the D-45 declaration in the spelling the
    /// design set uses rather than the one the struct happens to have.
    ///
    /// `evaluationPolicyVersion` is read from the constant rather than carried
    /// on the struct (D-162): it is a property of the gear that published, not
    /// of the publish, so a caller able to supply one could stamp a period with
    /// a semantics its rows were never frozen under. The three segments are
    /// stamped **flat**, beside the payload's other keys, because no document
    /// declares a `pricingSnapshotRef` envelope for this event and inventing
    /// one here would put a wire structure on the record that nothing agreed to.
    #[must_use]
    pub fn to_value(&self) -> JsonValue {
        json!({
            "planId": self.plan_id.get(),
            "revision": self.revision,
            "pendingVersionRef": self.pending_version_ref,
            "priceIds": self.price_ids,
            "evaluationPolicyVersion": EVALUATION_POLICY_GENERATION,
            "correlationId": self.correlation_id,
        })
    }
}

/// The two events a window **mutation** may emit.
///
/// §7 names four window events; the other two are the sweep's time-driven flips,
/// which are not publish units and must never carry a pending ref. Which of the
/// four a write emits is a fact only its caller has — `infra::window`'s
/// `Op::event` maps the route plane's three operations onto this pair — so the
/// restriction cannot live in the payload, and it is held here as a closed pair
/// instead of being checked after the fact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowMutationEvent {
    /// A schedule or an adjustment. Both emit one name: §7 declares no
    /// `PriceWindowAdjusted`, and the payload carries the new interval either
    /// way — see `infra::window::Op::event`.
    Scheduled,
    /// A cancellation.
    Cancelled,
}

impl WindowMutationEvent {
    /// The vocabulary name this mutation writes.
    #[must_use]
    pub const fn as_catalog_event(self) -> CatalogEvent {
        match self {
            Self::Scheduled => CatalogEvent::PriceWindowScheduled,
            Self::Cancelled => CatalogEvent::PriceWindowCancelled,
        }
    }
}

/// One event, as its writer is handed it.
///
/// `seq` is deliberately **not** here: it is the aggregate's, computed from the
/// rows already enqueued for it, and a caller that could supply one could
/// reorder another caller's events.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewOutboxEvent {
    /// The owning tenant.
    pub tenant_id: Uuid,
    /// The aggregate the event is ordered within — the plan, for a plan
    /// publish.
    pub aggregate_id: Uuid,
    /// Which of the frozen names — [`CatalogEvent::ALL`] is the roster.
    pub event: CatalogEvent,
    /// The rendered payload.
    pub payload: JsonValue,
    /// What makes a repeat of *this* publish the same event.
    pub dedup_key: String,
    /// The correlation id of the causing request.
    pub correlation_id: Uuid,
    /// When the event was enqueued, UTC — the caller's instant, for the reason
    /// [`NewAuditEntry`](super::audit_repo::NewAuditEntry) gives.
    pub enqueued_at: OffsetDateTime,
}

impl NewOutboxEvent {
    /// The `PlanCreated` event of one plan coming into existence
    /// (`inst-pa-return`).
    ///
    /// A named constructor for [`NewOutboxEvent::plan_published`]'s reason: the
    /// name, the aggregate and the dedup key are not the caller's to choose.
    #[must_use]
    pub fn plan_created(
        tenant_id: Uuid,
        payload: &PlanCreatedPayload,
        enqueued_at: OffsetDateTime,
    ) -> Self {
        Self {
            tenant_id,
            aggregate_id: payload.plan_id.get(),
            event: CatalogEvent::PlanCreated,
            payload: payload.to_value(),
            dedup_key: plan_created_dedup_key(payload.plan_id),
            correlation_id: payload.correlation_id,
            enqueued_at,
        }
    }

    /// The `PlanUpdated` event of one authoring edit (S2 §7, S10 §7).
    ///
    /// A named constructor for [`NewOutboxEvent::plan_published`]'s reason, and
    /// with more at stake than most: this is the one event of the plan plane
    /// whose act repeats, so a call site free to pick its own dedup key is a
    /// call site free to make the second edit of a draft dedup against the
    /// first. [`plan_updated_dedup_key`] is what stops that.
    #[must_use]
    pub fn plan_updated(
        tenant_id: Uuid,
        payload: &PlanUpdatedPayload,
        enqueued_at: OffsetDateTime,
    ) -> Self {
        Self {
            tenant_id,
            aggregate_id: payload.plan_id.get(),
            event: CatalogEvent::PlanUpdated,
            payload: payload.to_value(),
            dedup_key: plan_updated_dedup_key(
                payload.plan_id,
                payload.revision,
                payload.row_version,
            ),
            correlation_id: payload.correlation_id,
            enqueued_at,
        }
    }

    /// The `PlanPublished` event of one publish unit, whole.
    ///
    /// A named constructor rather than a struct literal at the call site,
    /// because three of the fields are not free: the event name is
    /// [`CatalogEvent::PlanPublished`], the aggregate is the plan, and the dedup
    /// key is [`plan_published_dedup_key`]. A call site free to choose them is a
    /// call site free to enqueue a `PlanPublished` under a dedup key that dedups
    /// against nothing.
    #[must_use]
    pub fn plan_published(
        tenant_id: Uuid,
        payload: &PlanPublishedPayload,
        enqueued_at: OffsetDateTime,
    ) -> Self {
        Self {
            tenant_id,
            aggregate_id: payload.plan_id.get(),
            event: CatalogEvent::PlanPublished,
            payload: payload.to_value(),
            dedup_key: plan_published_dedup_key(payload.plan_id, payload.revision),
            correlation_id: payload.correlation_id,
            enqueued_at,
        }
    }

    /// The `PlanRetired` event of one retirement (`inst-rt-event`).
    ///
    /// A named constructor for [`NewOutboxEvent::plan_published`]'s reason: the
    /// event name, the aggregate and the dedup key are not the caller's to
    /// choose. The aggregate is the **plan**, so the retirement is ordered with
    /// that plan's other events under the `(tenantId, aggregateId)` rule — which
    /// matters more here than anywhere else in this file, because a consumer
    /// that saw `PlanRetired` before the `PlanPublished` it follows would evict
    /// a plan it had never warmed.
    #[must_use]
    pub fn plan_retired(
        tenant_id: Uuid,
        payload: &PlanRetiredPayload,
        enqueued_at: OffsetDateTime,
    ) -> Self {
        Self {
            tenant_id,
            aggregate_id: payload.plan_id.get(),
            event: CatalogEvent::PlanRetired,
            payload: payload.to_value(),
            dedup_key: plan_retired_dedup_key(payload.plan_id, payload.revision),
            correlation_id: payload.correlation_id,
            enqueued_at,
        }
    }

    /// The `PlanMigrationScheduled` event of one migration schedule
    /// (`inst-ms-emit`, M2).
    ///
    /// The aggregate is the **source plan**, not the migration: `PlanRetired` and
    /// this event are the two things that can happen to a plan being sunset, and
    /// the `(tenantId, aggregateId)` ordering rule is what keeps a consumer from
    /// seeing the retirement before the migration that was supposed to move its
    /// subscribers off. A migration-keyed stream would order them independently.
    #[must_use]
    pub fn plan_migration_scheduled(
        tenant_id: Uuid,
        payload: &PlanMigrationScheduledPayload,
        enqueued_at: OffsetDateTime,
    ) -> Self {
        Self {
            tenant_id,
            aggregate_id: payload.source_plan_id.get(),
            event: CatalogEvent::PlanMigrationScheduled,
            payload: payload.to_value(),
            dedup_key: plan_migration_scheduled_dedup_key(payload.migration_id),
            correlation_id: payload.correlation_id,
            enqueued_at,
        }
    }

    /// The `PlanPublishDegraded` event of one publish whose subject has not
    /// warmed.
    ///
    /// A named constructor for [`NewOutboxEvent::plan_published`]'s reason: the
    /// event name, the aggregate and the dedup key are not the caller's to
    /// choose. The aggregate is the **plan**, so the degradation is ordered
    /// with that plan's other events under the `(tenantId, aggregateId)` rule
    /// rather than arriving on a stream of its own.
    #[must_use]
    pub fn plan_publish_degraded(
        tenant_id: Uuid,
        payload: &PlanPublishDegradedPayload,
        enqueued_at: OffsetDateTime,
    ) -> Self {
        Self {
            tenant_id,
            aggregate_id: payload.plan_id.get(),
            event: CatalogEvent::PlanPublishDegraded,
            payload: payload.to_value(),
            dedup_key: plan_publish_degraded_dedup_key(payload.plan_id, payload.catalog_version),
            correlation_id: payload.correlation_id,
            enqueued_at,
        }
    }

    /// The `PriceWindowActivated` of one window taking effect
    /// (`inst-ws-activate`).
    ///
    /// A named constructor for [`NewOutboxEvent::plan_published`]'s reason. What
    /// distinguishes this event from its sibling is entirely the **name and the
    /// dedup key**: the body is one rendering of one window's facts, and which
    /// boundary was crossed is what the name says (see
    /// [`PriceWindowTransitionPayload::to_value`] for why it is not also a field).
    ///
    /// The aggregate is the **plan** — §7 orders `PriceWindow*` events per
    /// `(tenant, plan)`, and the window's own id would give every window a
    /// stream of two events on which nothing is ordered against anything.
    #[must_use]
    pub fn price_window_activated(
        tenant_id: Uuid,
        payload: &PriceWindowTransitionPayload,
        enqueued_at: OffsetDateTime,
    ) -> Self {
        Self {
            tenant_id,
            aggregate_id: payload.plan_id.get(),
            event: CatalogEvent::PriceWindowActivated,
            payload: payload.to_value(),
            dedup_key: price_window_transition_dedup_key(
                CatalogEvent::PriceWindowActivated,
                payload.window_id,
            ),
            correlation_id: payload.correlation_id,
            enqueued_at,
        }
    }

    /// The `PriceWindowExpired` of one window reaching its end
    /// (`inst-ws-expire`).
    ///
    /// [`NewOutboxEvent::price_window_activated`]'s sibling, and the reason the
    /// dedup key names the transition: this event and that one are about one
    /// window, so a key that named only the window would let the activation
    /// swallow the expiry.
    #[must_use]
    pub fn price_window_expired(
        tenant_id: Uuid,
        payload: &PriceWindowTransitionPayload,
        enqueued_at: OffsetDateTime,
    ) -> Self {
        Self {
            tenant_id,
            aggregate_id: payload.plan_id.get(),
            event: CatalogEvent::PriceWindowExpired,
            payload: payload.to_value(),
            dedup_key: price_window_transition_dedup_key(
                CatalogEvent::PriceWindowExpired,
                payload.window_id,
            ),
            correlation_id: payload.correlation_id,
            enqueued_at,
        }
    }

    /// A window **mutation**'s event — `PriceWindowScheduled` or
    /// `PriceWindowCancelled` (D-99, `inst-ws-publishunit`).
    ///
    /// The other two of §7's four frozen names, and the last of the four to get a
    /// producer. Unlike its two siblings above it takes the event as an argument
    /// rather than being two constructors, and the reason is the inverse of theirs:
    /// there the event was a total function of *which sweep boundary* was crossed
    /// and a caller choosing it could have produced an `active` body under the
    /// expired name; here the event is a total function of **which of §5's three
    /// surfaces** was called, which is a fact only the caller has. `infra::window`'s
    /// `Op::event` decides it for that plane, a `const fn` over a closed
    /// enumeration; the four callers on the repricing, cutover, retirement and
    /// supersession paths name their own, which the parameter's type is what
    /// bounds.
    ///
    /// **The pair is a type rather than an assertion**: [`CatalogEvent`] is the
    /// gear's whole event vocabulary and carries the two time-driven flips as
    /// well, and a mutation emitting one of those would put a sweep's event in an
    /// operator's transaction — `inst-ws-publishunit`'s negative half inverted.
    /// This constructor is reached from the §5 routes, where the best a panic can
    /// become is a canonical 500 the out-of-process runtime unwinds it into — a fault with
    /// no code, on a request that was well formed. The restriction is a fact the
    /// compiler holds instead, so the wrong pairing is not reachable at all
    /// rather than reachable and answered badly.
    #[must_use]
    pub fn price_window_mutation(
        tenant_id: Uuid,
        event: WindowMutationEvent,
        payload: &PriceWindowTransitionPayload,
        enqueued_at: OffsetDateTime,
        act: &str,
    ) -> Self {
        Self {
            tenant_id,
            aggregate_id: payload.plan_id.get(),
            event: event.as_catalog_event(),
            payload: payload.to_value(),
            // The **act**, not the transition. `price_window_transition_dedup_key`
            // is the sweep's shape and says "one activation and one expiry per
            // window, by §4's edge set" — true of the two time-driven flips and
            // false of the operator's acts, because `Op::event` maps a schedule
            // **and** an adjustment to `PriceWindowScheduled`. So an adjustment of
            // a window that was scheduled through the route deduped against its
            // own schedule and was refused by `uq_pricing_outbox_dedup_key` — a
            // 409 on a legal act, and the reason no window could be adjusted twice.
            //
            // Widening the key rather than minting `PriceWindowAdjusted`: a fifth
            // event name is a wire fact §7 does not declare and
            // `chk_pricing_outbox_event_name` does not carry, so it owes a decision
            // and a migration. The dedup key is internal to this table, so naming
            // the act in it settles the defect without deciding the naming
            // question — which stays owed, and is now a naming gap rather than a
            // correctness one.
            dedup_key: format!(
                "{}/{act}",
                price_window_transition_dedup_key(event.as_catalog_event(), payload.window_id)
            ),
            correlation_id: payload.correlation_id,
            enqueued_at,
        }
    }

    /// The `PriceUpdated` of one row superseding another on its key
    /// (`inst-su-return`, D-88).
    ///
    /// A named constructor for [`NewOutboxEvent::plan_published`]'s reason, and the
    /// aggregate is the **plan** — Foundation §1.2 orders the frozen set per
    /// `(tenantId, aggregateId)`, and a price row's aggregate is the plan it prices,
    /// which is the same answer `audit_repo::plan_chain` gives one store over.
    /// `PriceCreated` for a row this gear has just authored (S3 §17.5).
    ///
    /// **The payload is deliberately shorter than [`PriceUpdatedPayload`]'s**, and
    /// the missing member is the informative one: a created row carries **no**
    /// `pending_version_ref`, because authoring is on the draft plane and a draft is
    /// addressable at no `CatalogVersion` at all. A supersession's `PriceUpdated`
    /// carries one because that act publishes. Filling this with a placeholder would
    /// be the lie this crate has already paid for once.
    #[must_use]
    pub fn price_created(
        tenant_id: Uuid,
        payload: &PriceCreatedPayload,
        enqueued_at: OffsetDateTime,
    ) -> Self {
        Self {
            tenant_id,
            aggregate_id: payload.plan_id.get(),
            event: CatalogEvent::PriceCreated,
            payload: payload.to_value(),
            dedup_key: price_created_dedup_key(payload.price_id),
            correlation_id: payload.correlation_id,
            enqueued_at,
        }
    }

    #[must_use]
    pub fn price_updated(
        tenant_id: Uuid,
        payload: &PriceUpdatedPayload,
        enqueued_at: OffsetDateTime,
    ) -> Self {
        Self {
            tenant_id,
            aggregate_id: payload.plan_id.get(),
            event: CatalogEvent::PriceUpdated,
            payload: payload.to_value(),
            dedup_key: price_updated_dedup_key(payload.price_id),
            correlation_id: payload.correlation_id,
            enqueued_at,
        }
    }

    /// The `BundleUpdated` of one composition normalisation (`inst-ba-return`,
    /// D-07).
    ///
    /// A named constructor for [`NewOutboxEvent::plan_published`]'s reason, and
    /// arriving late for a reason worth recording: this event and
    /// [`NewOutboxEvent::price_overlay_published`] were the crate's only
    /// from-scratch `NewOutboxEvent` literals until Z8-4, which is how the
    /// **name** came to be hand-spelled in a `format!` two lines under the enum
    /// that renders it, and how the key came to be separated with `:` where all
    /// ten of its siblings use `/`. Neither was a correctness break on its own;
    /// what the discipline was holding shut is Z8-3, and this event is the one
    /// that walked into it.
    ///
    /// The aggregate is the **plan** — a bundle rides a plan revision, so the
    /// composition change orders within that plan's stream. (The overlay event
    /// is the one place that differs, and its constructor says why.)
    #[must_use]
    pub fn bundle_updated(
        tenant_id: Uuid,
        payload: &BundleUpdatedPayload,
        enqueued_at: OffsetDateTime,
    ) -> Self {
        Self {
            tenant_id,
            aggregate_id: payload.plan_id.get(),
            event: CatalogEvent::BundleUpdated,
            payload: payload.to_value(),
            dedup_key: bundle_updated_dedup_key(
                payload.bundle_id,
                payload.plan_revision,
                payload.correlation_id,
            ),
            correlation_id: payload.correlation_id,
            enqueued_at,
        }
    }

    /// The `PriceOverlayPublished` of one overlay revision going live (D-248).
    ///
    /// A named constructor for [`NewOutboxEvent::plan_published`]'s reason, and
    /// [`NewOutboxEvent::bundle_updated`]'s late sibling — the two arrived
    /// together in Z8-4.
    ///
    /// **The aggregate is the overlay, not a plan**, which is the one place this
    /// differs from every other event in this module: an overlay is
    /// tenant-scoped and may target no plan at all (`target_plan_ids: []` is the
    /// global-scope case), so there is no plan stream to order it within.
    /// Per-overlay is also the right granularity — two revisions of one overlay
    /// must not be observed out of order, and two different overlays have no
    /// ordering relationship to keep.
    #[must_use]
    pub fn price_overlay_published(
        tenant_id: Uuid,
        payload: &PriceOverlayPublishedPayload,
        enqueued_at: OffsetDateTime,
    ) -> Self {
        Self {
            tenant_id,
            aggregate_id: payload.price_overlay_id,
            event: CatalogEvent::PriceOverlayPublished,
            payload: payload.to_value(),
            dedup_key: price_overlay_published_dedup_key(
                payload.price_overlay_id,
                payload.revision,
            ),
            correlation_id: payload.correlation_id,
            enqueued_at,
        }
    }
}

/// The dedup key of a plan publish: `PlanPublished/<plan_id>/<revision>`.
///
/// The publish unit itself, because a revision publishes exactly once. Written
/// down rather than computed at the call site so the same publish always
/// produces the same key and two different publishes never produce one.
#[must_use]
pub fn plan_published_dedup_key(plan_id: PlanId, revision: u64) -> String {
    format!(
        "{}/{}/{}",
        CatalogEvent::PlanPublished.as_str(),
        plan_id,
        revision
    )
}

/// The `PlanCreated` payload (`inst-pa-return`, S2 §7).
///
/// The field list is this module's, for its three siblings' reason: `inst-pa-return`
/// declares that the event is emitted and PRD §4 that it must be, and neither says
/// what it carries.
///
/// **No `skuId`, no shape.** A plan is announced at the instant it exists, before
/// any `PATCH` has attached a phase chain, an add-on set or a descriptor set, so a
/// payload that named the shape would name a shape nobody has authored yet. What a
/// consumer can act on here is that the aggregate now exists; the content arrives
/// as `PlanUpdated` and freezes at `PlanPublished`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanCreatedPayload {
    /// The plan that came into existence — the event's aggregate.
    pub plan_id: PlanId,
    /// The revision the create opened, which is `0` by construction.
    pub revision: u64,
    /// The correlation id of the causing request.
    pub correlation_id: Uuid,
}

impl PlanCreatedPayload {
    /// Render the payload for its `jsonb` column.
    #[must_use]
    pub fn to_value(&self) -> JsonValue {
        json!({
            "planId": self.plan_id.get(),
            "revision": self.revision,
            "correlationId": self.correlation_id,
        })
    }
}

/// What makes a repeat of one plan's creation the same event.
///
/// **The plan alone, with no revision segment.** A creation opens revision `0`
/// and a plan is created exactly once, so the revision would be a constant
/// dressed as a discriminator — and a constant in a dedup key is the shape that
/// makes two acts look distinct when they are not. [`plan_retired_dedup_key`]
/// carries a revision because a plan's *retirement* names the revision that
/// flipped, which is not always `0`.
#[must_use]
pub fn plan_created_dedup_key(plan_id: PlanId) -> String {
    format!("{}/{}", CatalogEvent::PlanCreated.as_str(), plan_id)
}

/// The `PlanUpdated` payload — the content change of one authored revision
/// (S2 §7, S10 §7).
///
/// The field list is this module's, as its siblings' are.
///
/// **It names the state the edit produced and not the edit's content.** A patch
/// is one of six facets — the plan columns, the phase chain, the add-on set, the
/// descriptor set, the derived meters, a bundle's composition — and a payload
/// that tried to describe which one moved would be a sixth enumeration of a set
/// this gear already enumerates in the audit trail, free to fall out of step
/// with it. What a consumer needs is that the draft moved and which state it
/// moved to; the *what* is `pricing_audit_log`'s, and the shape freezes at
/// publish anyway.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanUpdatedPayload {
    /// The plan whose draft moved — the aggregate the event is ordered within.
    pub plan_id: PlanId,
    /// The revision that was edited.
    pub revision: u64,
    /// The row version the edit **produced**, which is the caller's asserted
    /// version plus one: the compare-and-swap makes it unique to this mutation.
    pub row_version: u64,
    /// The correlation id of the causing request.
    pub correlation_id: Uuid,
}

impl PlanUpdatedPayload {
    /// Render the payload for its `jsonb` column.
    #[must_use]
    pub fn to_value(&self) -> JsonValue {
        json!({
            "planId": self.plan_id.get(),
            "revision": self.revision,
            "rowVersion": self.row_version,
            "correlationId": self.correlation_id,
        })
    }
}

/// What makes a repeat of one authoring edit the same event.
///
/// The revision **and the row version the edit produced**. Unlike every other
/// key in this file the act is repeatable — a draft takes as many edits as an
/// author cares to make — so the plan and the revision alone would collide on
/// the second one and the outbox would silently drop it. The post-state version
/// is the act's own identity, because the compare-and-swap that wrote it matched
/// on the predecessor and bumped by one: exactly one mutation of
/// `(plan, revision)` can ever have produced it.
#[must_use]
pub fn plan_updated_dedup_key(plan_id: PlanId, revision: u64, row_version: u64) -> String {
    format!(
        "{}/{}/{}/{}",
        CatalogEvent::PlanUpdated.as_str(),
        plan_id,
        revision,
        row_version
    )
}

/// The `PlanRetired` payload (`inst-rt-event`, D-128).
///
/// Shaped as [`PlanPublishedPayload`] is and for its reason: §7 freezes the
/// **name** `PlanRetired` and declares nothing about the field list, so the keys
/// below are this module's, written once so a later document has something
/// concrete to contradict.
///
/// It carries the **pending** version ref for the same reason `PlanPublished`
/// does. Retirement is a publish unit: the commit requests a handle, the
/// registry batches, and the version does not exist yet — so a consumer handed a
/// committed number here would be reading one this gear invented.
///
/// **No `priceIds`.** A retirement moves no price row: it flips the plan's
/// revision and, where D-51 lets it, cancels windows — and a cancelled window
/// announces itself as `PriceWindowCancelled` on its own aggregate. A price-id
/// list here would name rows this act did not touch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanRetiredPayload {
    /// The plan that retired.
    pub plan_id: PlanId,
    /// The revision that flipped — the plan's current one, which stays current
    /// (D-90, and `uq_pricing_plan_current` spans `retired`).
    pub revision: u64,
    /// The registry's pending handle.
    pub pending_version_ref: String,
    /// The windows the cancellation flow was invoked on. Empty whenever the
    /// D-79 presence lane could not answer, which D-182 makes this system's
    /// only case — the fail-closed posture keeps every window, so a consumer
    /// reading an empty list is reading "nothing was torn down", not "nothing
    /// was there".
    pub cancelled_window_ids: Vec<Uuid>,
    /// The correlation id of the causing request.
    pub correlation_id: Uuid,
}

impl PlanRetiredPayload {
    /// Render the payload for its `jsonb` column.
    ///
    /// `json!` rather than a derive, for [`PlanPublishedPayload::to_value`]'s
    /// stated reason: the wire vocabulary is spelled in one place and is visibly
    /// not the Rust field names.
    #[must_use]
    pub fn to_value(&self) -> JsonValue {
        json!({
            "planId": self.plan_id.get(),
            "revision": self.revision,
            "pendingVersionRef": self.pending_version_ref,
            "cancelledWindowIds": self.cancelled_window_ids,
            "correlationId": self.correlation_id,
        })
    }
}

/// The `PlanMigrationScheduled` payload — what Subscriptions needs to create
/// effective-dated `PlanLink`s (`inst-ms-emit`, `inst-mg-idem`, `inst-mg-boundary`,
/// M2, D-39).
///
/// The catalog emits; Subscriptions executes. Nothing here is a subscription's
/// state, because the catalog holds none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanMigrationScheduledPayload {
    /// The client-supplied schedule id, and **the idempotency key**: M2's dedup
    /// contract is `(migration_id, subscription)` and the consumer enforces it,
    /// so the id has to ride the event rather than be derivable from it.
    pub migration_id: Uuid,
    /// The retiring side.
    pub source_plan_id: PlanId,
    /// The source revision the schedule was computed against.
    pub source_revision: u64,
    /// The published target.
    pub target_plan_id: PlanId,
    /// When the migration takes effect.
    pub effective_at: OffsetDateTime,
    /// `all`, or the subscription filter the operator scoped it to.
    pub scope: JsonValue,
    /// The subscriptions excluded from the run — contract-locked, and never
    /// broken (`inst-cl-exclude`). Carried on the event because the executor must
    /// honour it, and because an empty list here would otherwise be
    /// indistinguishable from "no exclusions were computed".
    pub excluded_subscription_refs: Vec<Uuid>,
    /// Whether the lock registry could be asked at all (`inst-cl-source`).
    ///
    /// `true` says every subscription above is excluded because nobody could be
    /// asked rather than because Contracts named it — the fail-closed posture,
    /// which the absent registry makes this system's only case. A consumer that
    /// ignored this could read "everyone is contract-locked" as a fact about
    /// contracts.
    pub exclusions_unresolved: bool,
    /// **D-39, on the contract rather than in the consumer's head**: a migrated
    /// subscription enters the target's first **non-trial** phase. A migration
    /// never grants a new `trial`; entering an `interim` phase is allowed. Carried
    /// here because §3 step 2 puts the entry-phase rule on this event, and
    /// Subscriptions is what places the subscription.
    pub entry_phase_is_first_non_trial: bool,
    /// The correlation id of the causing request.
    pub correlation_id: Uuid,
}

impl PlanMigrationScheduledPayload {
    /// Render the payload for its `jsonb` column.
    ///
    /// `json!` rather than a derive, for [`PlanPublishedPayload::to_value`]'s
    /// stated reason.
    #[must_use]
    pub fn to_value(&self) -> JsonValue {
        json!({
            "migrationId": self.migration_id,
            "sourcePlanId": self.source_plan_id.get(),
            "sourceRevision": self.source_revision,
            "targetPlanId": self.target_plan_id.get(),
            "effectiveAt": format_rfc3339(self.effective_at),
            "scope": self.scope,
            "excludedSubscriptionRefs": self.excluded_subscription_refs,
            "exclusionsUnresolved": self.exclusions_unresolved,
            "entryPhaseIsFirstNonTrial": self.entry_phase_is_first_non_trial,
            "dedupContract": "(migrationId, subscription)",
            "correlationId": self.correlation_id,
        })
    }
}

/// What makes a repeat of one migration schedule the same event.
///
/// **The `migration_id` alone**, and deliberately not the effective date or the
/// scope: `inst-ms-api` makes the create idempotent on that id, so a retried
/// schedule is the *same* schedule and must not enqueue a second event.
/// `inst-mg-idem`'s re-trigger rides the same key for the same reason — the
/// consumer's own dedup is `(migration_id, subscription)`, and a second event
/// carrying a second key would defeat it before it was ever consulted.
#[must_use]
pub fn plan_migration_scheduled_dedup_key(migration_id: Uuid) -> String {
    format!(
        "{}/{}",
        CatalogEvent::PlanMigrationScheduled.as_str(),
        migration_id
    )
}

/// What makes a repeat of one plan's retirement the same event.
///
/// The plan and the revision it retired at. A plan retires **once** — the state
/// machine has no `retired -> published` edge — so this is the act's own
/// identity, exactly as [`plan_published_dedup_key`] is the publish's.
#[must_use]
pub fn plan_retired_dedup_key(plan_id: PlanId, revision: u64) -> String {
    format!(
        "{}/{}/{}",
        CatalogEvent::PlanRetired.as_str(),
        plan_id,
        revision
    )
}

/// The `PlanPublishDegraded` payload — the second event this file defines, and
/// the second whose field list no document declares.
///
/// §1.2 requires the event and names nothing about its shape, exactly as it did
/// for `PlanPublished`; the keys below are therefore this module's, written
/// here once so a later document has something concrete to contradict.
///
/// It names the publish by its **plan and committed version** and carries the
/// handle as lineage. Under D-166 the degraded condition is *the commit was
/// observed and the warm has not landed*, so the version is known at every
/// instant the condition is observable — which is what the earlier
/// handle-keyed form could not assume, having been written when the degraded
/// predicate fired on a ref nobody had resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanPublishDegradedPayload {
    /// The plan whose subject is not warm.
    pub plan_id: PlanId,
    /// The version the registry committed the publish into.
    pub catalog_version: CatalogVersion,
    /// The registry handle its publish is still waiting on — lineage, so an
    /// operator can find the ref row this was observed from.
    pub pending_version_ref: String,
    /// When this gear first saw the registry's answer: **what the degraded age
    /// is measured from** (D-166 clause 2).
    ///
    /// `requested_at` was here before and was the wrong instant, not merely a
    /// coarser one: `fr-publish-fanout-atomicity` puts the pre-commit batching
    /// wait **outside** degraded handling, and an age measured from the request
    /// includes nothing but that wait for the first five minutes of every
    /// publish.
    pub commit_observed_at: OffsetDateTime,
    /// The correlation id of the sweep pass that observed it.
    pub correlation_id: Uuid,
}

impl PlanPublishDegradedPayload {
    /// Render the payload for its `jsonb` column, `camelCase` as its sibling's.
    #[must_use]
    pub fn to_value(&self) -> JsonValue {
        json!({
            "planId": self.plan_id.get(),
            "catalogVersion": self.catalog_version.get(),
            "pendingVersionRef": self.pending_version_ref,
            "commitObservedAt": format_rfc3339(self.commit_observed_at),
            "correlationId": self.correlation_id,
        })
    }
}

/// The dedup key of a degraded publish:
/// `PlanPublishDegraded/<plan_id>/<catalog_version>`.
///
/// **The plan and its version**, which is what D-166 clause (2) makes available:
/// the degraded condition is now *observed committed and still unwarm*, so the
/// pass holding the condition is the pass holding the registry's answer. The
/// earlier form keyed on the pending handle because the predicate it served
/// fired on refs nothing had resolved — there was no version to name.
///
/// Naming the version rather than the handle is also what makes the key mean the
/// **publish**: two revisions of one plan landing in one D-47 batch degrade as
/// one version's failure to warm, and an operator paging on it acts once.
///
/// Under `uq_pricing_outbox_dedup_key (tenant_id, dedup_key)` that makes a
/// repeat of one degradation **one** event, which is the whole requirement.
#[must_use]
pub fn plan_publish_degraded_dedup_key(plan_id: PlanId, catalog_version: CatalogVersion) -> String {
    format!(
        "{}/{}/{}",
        CatalogEvent::PlanPublishDegraded.as_str(),
        plan_id,
        catalog_version.get()
    )
}

/// The `PriceWindowActivated` / `PriceWindowExpired` payload — one type for both,
/// and now **one rendering** for both: they are one fact about one window, and
/// which boundary it crossed is the event's name.
///
/// **The third payload whose field list no document declares.** §7 names the
/// four `PriceWindow*` events, says they are ordered per `(tenant, plan)`,
/// idempotency-keyed and at-least-once, and says nothing about their content.
/// The keys below are therefore this module's, in the `camelCase` the design set
/// spells consumer-visible fields in, written here once so a later document has
/// something concrete to contradict — the posture this file already took twice.
///
/// It carries the window's **interval** rather than the instant of the flip, and
/// that is the same choice D-99 makes one plane over: the interval is the fact,
/// and "active at `t`" is derived from it. The flip instant is not dropped — it
/// is `effectiveFrom` for an activation and `effectiveTo` for an expiry by
/// construction of §4's conditions, so a field for it would be a second
/// spelling of a value already here, free to disagree with it. When the flip was
/// *recorded* is the row's own `enqueued_at`, which is where the sweep's lag is
/// visible.
///
/// # `state` is gone, because that rule was applied to one of two candidates
///
/// The payload used to carry `"state": "active" | "expired"`, and the paragraph
/// above refuses the flip instant on a ground that covers it exactly: a second
/// spelling of a value already carried, free to disagree with it. `state` was a
/// **total function of the event name** — the two constructors are the only
/// callers and there is one activation and one expiry per window by §4's edge set
/// — so the rule is applied consistently and the field is dropped rather than
/// kept and tested.
///
/// The counter-argument, and why it does not save the field: the flip instant
/// would have duplicated a value *inside* the payload, while `state` duplicated
/// the **envelope** — `pricing_outbox.event_name`, which is not in this JSON. That
/// is a real difference and it makes the duplication *worse*, not admissible. The
/// event name is `NOT NULL`, pinned to the frozen set by
/// `chk_pricing_outbox_event_name`, and is what any consumer dispatches on; a body
/// contradicting its own envelope has no resolution rule at all, whereas two
/// disagreeing payload keys at least both live in one document. A test asserting
/// the pairing was the other option and is not equivalent: the rule as this doc
/// block states it refuses a second spelling because it *can* disagree, not
/// because nothing checks it.
///
/// **The design set constrains neither choice**: §7 names the four `PriceWindow*`
/// events, their `(tenant, plan)` ordering, their idempotency keys and their
/// at-least-once delivery, and declares no payload field — no `state`, no
/// `windowId`. Nothing is owed to a consumer either way, no relay existing. So
/// dropping the field mints nothing and removes nothing anything agreed to; it is
/// decided by the rule this file already wrote down.
///
/// It carries **no canonical scope key**. The key is ten axes of
/// `pricing_price` and resolving one per event would put a query per flip on a
/// sweep's path; a consumer that needs the key has the `priceId` and the pinned
/// delta, which carries the window facts grouped by key already (D-121).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PriceWindowTransitionPayload {
    /// The window that moved.
    pub window_id: Uuid,
    /// The plan the window's row is on — the aggregate the event is ordered
    /// within.
    pub plan_id: PlanId,
    /// The price row the window is bound to.
    pub price_id: Uuid,
    /// Inclusive start of the half-open interval, UTC.
    pub effective_from: OffsetDateTime,
    /// Exclusive end, UTC; `None` is open-ended.
    pub effective_to: Option<OffsetDateTime>,
    /// The correlation id of the sweep pass that observed the boundary.
    pub correlation_id: Uuid,
}

impl PriceWindowTransitionPayload {
    /// Render the payload for its `jsonb` column, `camelCase` as its two siblings'.
    ///
    /// A `to_value(&self)` like theirs, which it could not be while the renderer
    /// took a `state`: a public renderer the caller chose a state for could have
    /// produced an `active` body under the `PriceWindowExpired` name, so the
    /// pairing had to be held by a private free function and its two callers. With
    /// the field gone there is no pairing left to get wrong, and the guard is a
    /// value that does not exist rather than a shape a reader has to notice.
    #[must_use]
    pub fn to_value(&self) -> JsonValue {
        json!({
            "windowId": self.window_id,
            "planId": self.plan_id.get(),
            "priceId": self.price_id,
            "effectiveFrom": format_rfc3339(self.effective_from),
            "effectiveTo": self.effective_to.map(format_rfc3339),
            "correlationId": self.correlation_id,
        })
    }
}

/// The dedup key of one window transition:
/// `PriceWindowActivated/<window_id>`.
///
/// **The window *and* the transition**, which is what makes the coordination
/// lease over the activation sweep a performance measure rather than the only
/// thing standing between two replicas and two events for one flip. The event
/// name *is* the transition — there is exactly one activation and one expiry per
/// window, by §4's edge set — so naming it plus the window id covers the pair
/// with no third segment.
///
/// Both halves are load-bearing and each fails a different way:
///
/// * without the **window**, one plan's second activation would dedup against
///   its first;
/// * without the **transition**, a window's expiry would dedup against its own
///   activation and never be emitted at all — the event a consumer needs in
///   order to stop resolving a price.
///
/// Two sweeps flipping one window therefore write **one** row, refused by either
/// member of the pair this key reaches: `uq_pricing_outbox_dedup_key
/// (tenant_id, dedup_key)` and the `outbox_id` primary key, which [`outbox_id`]
/// derives from the same pair for exactly this reason. Naming only the index would
/// name the one the driver cannot confirm answered.
#[must_use]
pub fn price_window_transition_dedup_key(event: CatalogEvent, window_id: Uuid) -> String {
    format!("{}/{}", event.as_str(), window_id)
}

/// The `PriceCreated` payload — a row coming into existence on a key.
///
/// **Two producers, one name**, which is D-203's decision rather than a
/// duplication: the authoring door, where `03-price-structure.md` §17.5 puts it,
/// and the cutover's commit, which R-02 requires to announce it twice. Both are a
/// row being born; neither is the other's special case. This type's field list is
/// this module's, like its three siblings', because §17.5 declares what the event
/// *means* and not what it carries.
///
/// It carries **no** version ref, and that is the difference from
/// [`PriceUpdatedPayload`] worth stating: a draft is addressable at no
/// `CatalogVersion`, so there is no pending handle to name from the authoring door.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PriceCreatedPayload {
    /// The plan the row belongs to, and the event's aggregate.
    pub plan_id: PlanId,
    /// The row that was authored.
    pub price_id: Uuid,
    /// Its canonical scope key, rendered.
    pub scope_key: String,
    /// The request that authored it.
    pub correlation_id: Uuid,
}

impl PriceCreatedPayload {
    /// The payload as the outbox stores it.
    #[must_use]
    pub fn to_value(&self) -> JsonValue {
        serde_json::json!({
            "planId": self.plan_id.get(),
            "priceId": self.price_id,
            "scopeKey": self.scope_key,
            "correlationId": self.correlation_id,
        })
    }
}

/// The `PriceUpdated` payload — the **fourth** whose field list no document
/// declares.
///
/// `03-price-structure.md` §17.5 says only *"`PriceCreated` on row authoring,
/// `PriceUpdated` on supersession"*, and Foundation §1.2 puts both in the frozen
/// name set ordered per `(tenantId, aggregateId)`. So what the event *means* is
/// declared and its content is not; the keys below are this module's, in the same
/// `camelCase` its three siblings use.
///
/// **A doc block that has to explain which type it belongs to has already been
/// transplanted.** Inserting a sibling type between a doc and the type it describes
/// silently re-parents the block, and the symptom invites the wrong fix — an
/// `#[allow(clippy::doc_markdown)]` carrying a `reason` that explains which type
/// the block above documents. This one has been through it, carrying
/// *"`PriceCreated` still has no producer anywhere in this gear"* into a file the
/// same move had given it one in.
///
/// # It names the predecessor, and the field is not optional
///
/// A `PriceUpdated` is *by definition* a row landing on an occupied canonical scope
/// key: both sanctioned producers of `published → superseded` set
/// `supersedes_price_id` on the successor (D-127), and there is no third. A
/// `Option<Uuid>` here would be a shape saying a price can be "updated" with nothing
/// updated — and a consumer's whole reason to read this event is to stop resolving
/// the row it replaces.
///
/// # It carries the **canonical scope key**, unlike its window sibling
///
/// [`PriceWindowTransitionPayload`] deliberately does not, on the ground that
/// resolving ten axes per event would put a query on a *sweep's* path. This event
/// is emitted from the operator transaction that already holds the key, so the
/// argument does not transfer: the cost is zero and the key is the only field that
/// tells a consumer *which price* moved without a second lookup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PriceUpdatedPayload {
    /// The plan the row belongs to — the aggregate the event is ordered within.
    pub plan_id: PlanId,
    /// The row that became current on the key.
    pub price_id: Uuid,
    /// The ten axes, canonically rendered.
    pub scope_key: String,
    /// The row it replaced, which every producer of this event has.
    pub supersedes_price_id: Uuid,
    /// The instant coverage hands over from the predecessor to this row.
    pub changeover: OffsetDateTime,
    /// The registry's **pending** handle, for [`PlanPublishedPayload`]'s reason: the
    /// version does not exist yet and will not until the registry batches (D-47).
    pub pending_version_ref: String,
    /// The correlation id of the causing request.
    pub correlation_id: Uuid,
}

impl PriceUpdatedPayload {
    /// Render the payload for its `jsonb` column, `camelCase` as its siblings'.
    #[must_use]
    pub fn to_value(&self) -> JsonValue {
        json!({
            "planId": self.plan_id.get(),
            "priceId": self.price_id,
            "scopeKey": self.scope_key,
            "supersedesPriceId": self.supersedes_price_id,
            "changeover": format_rfc3339(self.changeover),
            "pendingVersionRef": self.pending_version_ref,
            "correlationId": self.correlation_id,
        })
    }
}

/// The dedup key of one row's supersession: `PriceUpdated/<price_id>`.
///
/// **The successor's id, and nothing else.** `draft → published` is a row's only
/// edge out of `draft` and the supersession commit's compare-and-swap refuses the
/// second attempt, so a row is superseded-onto exactly once and its id is the
/// natural key — the same argument [`plan_published_dedup_key`] makes about a
/// revision.
///
/// It is stable across a **retry** of one act for a reason worth stating, because
/// the analogous window key was not: the successor row is staged at compose and its
/// id is read back from the store by every later attempt, so a retry after an
/// approval renders this key from the same id rather than from one minted per
/// request. That is the property `window_unit_ref`'s schedule arm could not have,
/// and it is why this key needs no act segment the way
/// [`NewOutboxEvent::price_window_mutation`]'s does.
#[must_use]
pub fn price_updated_dedup_key(price_id: Uuid) -> String {
    format!("{}/{}", CatalogEvent::PriceUpdated.as_str(), price_id)
}

/// One `PriceCreated` per price row, ever.
///
/// The row id is the whole key because a row is created exactly once: a replayed
/// authoring call is refused upstream by the idempotency gate, and a re-authored
/// key is a **different** row with a different id.
#[must_use]
pub fn price_created_dedup_key(price_id: Uuid) -> String {
    format!("{}/{}", CatalogEvent::PriceCreated.as_str(), price_id)
}

/// The `BundleUpdated` payload — one composition, as a consumer is told about it
/// (`design/08-bundles.md` §2, `inst-ba-return`).
///
/// Its field list is this module's, like its four siblings': §7 freezes the
/// **name** `BundleUpdated` and declares nothing about the body.
///
/// The correlation is **not** rendered into it — it rides the row's own column,
/// which is where a consumer reads it. That is the wire shape this event already
/// had rather than a decision taken here; `PlanPublishedPayload` renders it as
/// well, and reconciling the two is a wire question and not this change's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleUpdatedPayload {
    /// The bundle whose composition moved.
    pub bundle_id: Uuid,
    /// The plan it rides — the aggregate the event is ordered within.
    pub plan_id: PlanId,
    /// The plan revision the composition belongs to.
    pub plan_revision: u64,
    /// How the bundle arrives at its price (`inst-bb-declared`).
    pub price_basis: PriceBasis,
    /// How the invoice lays it out.
    pub invoice_itemization: InvoiceItemization,
    /// The correlation id of the causing request, which is also the act segment
    /// of [`bundle_updated_dedup_key`].
    pub correlation_id: Uuid,
}

impl BundleUpdatedPayload {
    /// Render the payload for its `jsonb` column, `camelCase` as its siblings'.
    #[must_use]
    pub fn to_value(&self) -> JsonValue {
        json!({
            "bundleId": self.bundle_id,
            "planId": self.plan_id.get(),
            "planRevision": self.plan_revision,
            "priceBasis": self.price_basis.as_str(),
            "invoiceItemization": self.invoice_itemization.as_str(),
        })
    }
}

/// The dedup key of one bundle composition announcement:
/// `BundleUpdated/<bundle_id>/<plan_revision>/<act>`.
///
/// **The act, and not only the revision** — Z8-3. This key read
/// `BundleUpdated:<bundle>:<revision>` and so asserted
/// [`plan_published_dedup_key`]'s argument, *"a revision publishes exactly
/// once"*. That argument rests on the publish commit's compare-and-swap refusing
/// the second attempt, and `BundleService::publish_composition` has no swap: it
/// re-reads the composition, re-normalises and enqueues, and the route admits a
/// second call at one `plan_revision` — a composition edit moves the revision's
/// `row_version`, which voids the content pin, so the legal republish is another
/// approved unit over the same revision. `uq_pricing_outbox_dedup_key` was
/// therefore the only thing standing there and it answered
/// `CONCURRENT_MUTATION`: a 409 meaning *retry* on an act whose every retry
/// collides identically. Widening the key is
/// [`NewOutboxEvent::price_window_mutation`]'s remedy for the same defect one
/// event over, taken for the same reason — the key is internal to this table, so
/// naming the act in it settles the defect without deciding any wire question.
///
/// **The act is the correlation id**, because D-178 clause (2) binds one such
/// value to one operator call and `api::rest::correlation::require_correlation`
/// refuses to mint anywhere but the edge — so one announcement per correlation is
/// one per call. The residue: a resend at the *client* level is a new request, so
/// a new correlation, so a second content-identical event. That is what the PRD's
/// *"carrying correlation/idempotency keys so consumers can dedupe"* already puts
/// on the consumer under at-least-once delivery. Keying on a digest of the
/// normalised composition instead would dedup that repeat and answer it
/// `CONCURRENT_MUTATION` again, which is the defect.
#[must_use]
pub fn bundle_updated_dedup_key(bundle_id: Uuid, plan_revision: u64, act: Uuid) -> String {
    format!(
        "{}/{}/{}/{}",
        CatalogEvent::BundleUpdated.as_str(),
        bundle_id,
        plan_revision,
        act
    )
}

/// The `PriceOverlayPublished` payload — the shard key a consumer re-reads by
/// (D-248).
///
/// The field list is D-248's own: `priceOverlayId`, `revision`, `scopeClass`,
/// `scopeValue`, `precedence`, `pendingVersionRef`, `correlationId`. The scope
/// pair is the `overlay_index` shard key (D-112), so a consumer knows which index
/// document to re-read without a lookup, and `precedence` is the order it
/// resolves by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PriceOverlayPublishedPayload {
    /// The overlay that published — and the aggregate, unlike every sibling.
    pub price_overlay_id: Uuid,
    /// The revision that became current.
    pub revision: u64,
    /// Class and value together, so the two cannot be rendered out of step: the
    /// store spends a biconditional `CHECK` on the pairing and
    /// [`ScopeSelector`](crate::domain::overlay::ScopeSelector) makes the
    /// mismatch unrepresentable.
    pub scope: ScopeSelector,
    /// The precedence the index resolves by.
    pub precedence: i32,
    /// The registry's **pending** handle, for [`PlanPublishedPayload`]'s reason:
    /// the version does not exist yet and will not until the registry batches
    /// (D-47).
    pub pending_version_ref: String,
    /// The correlation id of the causing request.
    pub correlation_id: Uuid,
}

impl PriceOverlayPublishedPayload {
    /// Render the payload for its `jsonb` column, `camelCase` as its siblings'.
    #[must_use]
    pub fn to_value(&self) -> JsonValue {
        json!({
            "priceOverlayId": self.price_overlay_id,
            "revision": self.revision,
            "scopeClass": self.scope.class().as_str(),
            "scopeValue": self.scope.value().map(ScopeValue::as_str),
            "precedence": self.precedence,
            "pendingVersionRef": self.pending_version_ref,
            "correlationId": self.correlation_id,
        })
    }
}

/// The dedup key of one overlay publish:
/// `PriceOverlayPublished/<price_overlay_id>/<revision>`.
///
/// [`plan_published_dedup_key`]'s argument, and here it **holds**: the commit
/// proves the target is the open draft before anything is written and flips it to
/// `published`, so a second publish of one revision is refused with a
/// `NotFound` — by a compare-and-swap, before the enqueue — which is exactly the
/// property [`bundle_updated_dedup_key`] lacks and therefore why that key carries
/// an act segment and this one does not.
///
/// D-248 and `09-price-overlays.md` §4 write the key as
/// `PriceOverlayPublished:<overlay_id>:<revision>`, with colons. The separator is
/// `/` here, as in all ten sibling keys, and the dedup key is internal to this
/// table — so the two documents' spelling is stale prose about a private key
/// rather than a contract, and it is owed a docs-wave correction.
#[must_use]
pub fn price_overlay_published_dedup_key(price_overlay_id: Uuid, revision: u64) -> String {
    format!(
        "{}/{}/{}",
        CatalogEvent::PriceOverlayPublished.as_str(),
        price_overlay_id,
        revision
    )
}

/// Enqueue one event inside `runner`'s transaction, returning the `seq` it
/// landed at within its aggregate.
///
/// # Errors
/// [`RepoError::ConcurrentMutation`] naming the aggregate the event is ordered
/// within (`contention_aggregate`) for `uq_pricing_outbox_sequence` alone —
/// losing the race for an aggregate's next `seq`, which is D-159's second
/// serialization point and renders `CONCURRENT_MUTATION`, 409, retriable.
/// [`RepoError::OutboxEventAlreadyEnqueued`] for `uq_pricing_outbox_dedup_key`, a
/// second event on one `(tenant, dedup key)`, which renders
/// `OUTBOX_EVENT_ALREADY_ENQUEUED`, 409, and is permanent: the retry builds the
/// same key from the same inputs. Neither is pre-checked, because a pre-check is a
/// read the insert races with anyway; [`contention_or_db`] at the insert decides
/// between the second and [`RepoError::Db`].
///
/// [`RepoError::Db`] on any other scope or storage failure.
/// [`RepoError::CorruptRow`] when the aggregate's greatest `seq` is not a
/// position this counter can count in or has no successor.
pub async fn enqueue(
    runner: &impl DBRunner,
    scope: &AccessScope,
    event: NewOutboxEvent,
) -> Result<u64, RepoError> {
    let seq = match read_head_seq(runner, scope, event.tenant_id, event.aggregate_id).await? {
        None => 0_u64,
        Some(head) => head.checked_add(1).ok_or_else(|| {
            RepoError::CorruptRow(format!(
                "pricing_outbox aggregate {} stands at seq {head}, which has no successor",
                event.aggregate_id
            ))
        })?,
    };
    let stored_seq = i64::try_from(seq).map_err(|_| {
        RepoError::CorruptRow(format!(
            "pricing_outbox aggregate {} reached seq {seq}, which its column cannot hold",
            event.aggregate_id
        ))
    })?;

    let am = outbox::ActiveModel {
        outbox_id: Set(outbox_id(event.tenant_id, &event.dedup_key)),
        tenant_id: Set(event.tenant_id),
        aggregate_id: Set(event.aggregate_id),
        event_name: Set(event.event.as_str().to_owned()),
        seq: Set(stored_seq),
        payload: Set(event.payload.clone()),
        dedup_key: Set(event.dedup_key.clone()),
        correlation_id: Set(event.correlation_id),
        enqueued_at: Set(event.enqueued_at),
        // The relay drains; the publish commit never does.
        published_at: Set(None),
    };
    outbox::Entity::insert(am.clone())
        .secure()
        .scope_with_model(scope, &am)
        .map_err(|e| RepoError::Db(format!("pricing_outbox scope: {e}")))?
        .exec(runner)
        .await
        .map_err(|e| {
            // **The two unique indexes on this table are told apart here, because
            // they mean opposite things.** `uq_pricing_outbox_sequence` over
            // `(tenant_id, aggregate_id)` is D-159's second serialization point: a
            // race another writer won, and a retry resolves it.
            // `uq_pricing_outbox_dedup_key` is the deduplication doing its job on a
            // repeated act, and it is deterministic — the retry renders the same key
            // from the same inputs and collides identically. One class for both can
            // only be `CONCURRENT_MUTATION`, whose remedy is *retry*, and under it
            // the caller's whole transaction rolls back forever on the deterministic
            // half. So the index that refused is the fact this arm recovers.
            if e.is_unique_violation() && names_the_dedup_key(&e.to_string()) {
                return RepoError::OutboxEventAlreadyEnqueued {
                    dedup_key: event.dedup_key.clone(),
                };
            }
            contention_or_db(&e, &contention_aggregate(&event), "enqueue pricing_outbox")
        })?;
    Ok(seq)
}

/// Does this driver message name the **dedup** key rather than the sequence index?
///
/// A spelling per engine per index, because the two engines phrase a unique
/// violation differently — the shape `bundle_repo::names_the_bundle_plan_slot`
/// already uses.
///
/// **And a repeated act violates two indexes at once.** `outbox_id` is
/// [`outbox_id`]'s `v5` digest of `(tenant_id, dedup_key)` and it is
/// the table's primary key, so the two constraints have the same operand: a
/// `pricing_outbox_pkey` violation *is* a dedup-key repeat, and there is no input
/// that trips one without the other. Which name comes back is the engine's choice
/// and not a fact about the act — Postgres checks in index OID order and the
/// primary key is created with the table, `SQLite` names the columns of whichever
/// it evaluated — so recognising only the dedup index answers correctly on one
/// engine and falls through to the retriable `CONCURRENT_MUTATION` on the other,
/// which is the exact advice [`RepoError::OutboxEventAlreadyEnqueued`] exists to
/// stop giving.
fn names_the_dedup_key(message: &str) -> bool {
    message.contains("uq_pricing_outbox_dedup_key")
        || message.contains("pricing_outbox.dedup_key")
        || message.contains("pricing_outbox_pkey")
        || message.contains("pricing_outbox.outbox_id")
}

/// The aggregate a losing writer is told to retry against, as D-159 requires the
/// 409 to name it.
///
/// **A match on the event, not one noun for all of them.** `aggregate_id` is the
/// plan for thirteen of the fourteen events and the **overlay** for
/// `PriceOverlayPublished` — [`NewOutboxEvent::price_overlay_published`] says so
/// in as many words, *"the aggregate is the overlay, not a plan, which is the one
/// place this differs from every other event in this module"*. The single
/// `format!("plan {}", …)` this replaced put that overlay id into
/// [`RepoError::ConcurrentMutation`]`{ aggregate }`, which `repo_failure` renders
/// straight into the `CONCURRENT_MUTATION` body, so a losing overlay publish was
/// told to retry against a plan id that names no plan.
///
/// The match is **exhaustive on purpose**. A fifteenth event added to
/// [`CatalogEvent`] cannot inherit a noun by omission: it does not compile until
/// somebody says which aggregate it is ordered within, which is the same question
/// its constructor has to answer when it picks an `aggregate_id`.
fn contention_aggregate(event: &NewOutboxEvent) -> String {
    let aggregate = match event.event {
        CatalogEvent::PlanCreated
        | CatalogEvent::PlanUpdated
        | CatalogEvent::PlanPublished
        | CatalogEvent::PlanRetired
        | CatalogEvent::PlanMigrationScheduled
        | CatalogEvent::PlanPublishDegraded
        | CatalogEvent::BundleUpdated
        | CatalogEvent::PriceCreated
        | CatalogEvent::PriceUpdated
        | CatalogEvent::PriceWindowScheduled
        | CatalogEvent::PriceWindowActivated
        | CatalogEvent::PriceWindowExpired
        | CatalogEvent::PriceWindowCancelled => "plan",
        CatalogEvent::PriceOverlayPublished => "price overlay",
    };
    format!("{aggregate} {}", event.aggregate_id)
}

/// The greatest `seq` already enqueued for `(tenant_id, aggregate_id)`.
async fn read_head_seq(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    aggregate_id: Uuid,
) -> Result<Option<u64>, RepoError> {
    let row = outbox::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(outbox::Column::TenantId.eq(tenant_id))
                .add(outbox::Column::AggregateId.eq(aggregate_id)),
        )
        .order_by(outbox::Column::Seq, Order::Desc)
        .limit(1)
        .one(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read outbox head: {e}")))?;
    row.map(|row| {
        u64::try_from(row.seq).map_err(|e| {
            RepoError::CorruptRow(format!(
                "pricing_outbox aggregate {aggregate_id} holds seq {}: {e}",
                row.seq
            ))
        })
    })
    .transpose()
}

/// The row's surrogate key, derived from the identity the table actually
/// states: `UNIQUE (tenant_id, dedup_key)`.
///
/// `price_repo`'s `band_id` argument, applied one table over. `outbox_id` is a
/// `PRIMARY KEY` with no default and nothing outside this module reads it, so it
/// could have been random — deriving it makes the surrogate agree with the real
/// identity, so a repeat of one publish collides on **both** keys rather than on
/// only the one that happened to be checked first.
fn outbox_id(tenant_id: Uuid, dedup_key: &str) -> Uuid {
    Uuid::new_v5(&tenant_id, dedup_key.as_bytes())
}

#[cfg(test)]
#[path = "outbox_repo_tests.rs"]
mod outbox_repo_tests;
