//! `IngestService` (`DESIGN.md:623-639`): producer-facing, owns the write
//! path.

use crate::domain::model::Sequence;
use async_trait::async_trait;
use authz_resolver_sdk::{AccessRequest, PolicyEnforcer};
use gts::{GtsId, GtsIdPattern, GtsTypeId};
use toolkit::domain_model;
use toolkit_gts::GtsInstanceId;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::authz::{EVENT_TYPE_RESOURCE, tenant_authorized};
use crate::domain::backend::BackendResolver;
use crate::domain::error::DomainError;
use event_broker_sdk::models::EventType;

use crate::domain::model::{Event, Meta};

/// One event as a producer submits it: what the publish schema admits, and
/// nothing the broker derives.
///
/// A publish body names neither a topic nor a partition key. The stream an
/// event belongs to is the `topic` trait on its event type, and which member
/// determines its partition is that type's `partition_key` pointer, so an event
/// can never disagree with its type about either and there is nothing for the
/// broker to cross-check. [`IngestService::publish_event`] resolves both and
/// returns the [`Event`] the request became, with the topic and the selected
/// partition stamped on it.
#[domain_model]
#[derive(Debug, Clone)]
pub struct PublishRequest {
    pub id: Uuid,
    pub r#type: GtsTypeId,
    pub tenant_id: Uuid,
    pub source: String,
    pub subject: String,
    pub subject_type: String,
    pub occurred_at: chrono::DateTime<chrono::Utc>,
    pub trace_parent: Option<String>,
    pub data: serde_json::Value,
    pub meta: Option<Meta>,
}

/// Result of a batch publish - which events were accepted (in submission
/// order) and which were rejected with a reason.
#[domain_model]
#[derive(Debug, Clone, Default)]
pub struct BatchResult {
    pub accepted: Vec<Uuid>,
    pub failed: Vec<(Uuid, String)>,
}

/// Dedup protocol mode a producer registers under (`ADR-0004`).
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProducerMode {
    Chained,
    Monotonic,
}

/// Input to `IngestService::register_producer` - this codebase's convention
/// is a request struct per multi-field operation (`ConsumerGroupCreateInput`,
/// `JoinRequest`), not positional loose args.
#[domain_model]
#[derive(Debug, Clone)]
pub struct ProducerRegistrationInput {
    pub mode: ProducerMode,
    pub client_agent: String,
}

/// Result of `IngestService::register_producer` - echoes the identity fields
/// `docs/openapi.yaml`'s `POST /v1/producers` response requires.
#[domain_model]
#[derive(Debug, Clone)]
pub struct ProducerRegistration {
    pub id: Uuid,
    pub mode: ProducerMode,
    pub client_agent: String,
}

/// One partition's `last_sequence` cursor.
#[domain_model]
#[derive(Debug, Clone, Copy)]
pub struct ProducerPartitionCursor {
    pub partition: i32,
    pub last_sequence: i64,
}

/// Cursors for one topic - the partitions a producer has published to.
#[domain_model]
#[derive(Debug, Clone)]
pub struct ProducerTopicCursors {
    pub topic: String,
    pub partitions: Vec<ProducerPartitionCursor>,
}

/// Result of `IngestService::get_producer_cursors` (`docs/openapi.yaml`'s
/// `GET /v1/producers/{id}/cursors` response) - an object (not a bare
/// array) so the echoed identity fields ride along and the shape stays
/// extensible.
#[domain_model]
#[derive(Debug, Clone, Default)]
pub struct ProducerCursors {
    pub producer_id: Uuid,
    pub client_agent: String,
    pub topics: Vec<ProducerTopicCursors>,
}

/// Which `evbk_producer_state` rows `IngestService::reset_producer` clears.
/// Not `Option<(String, i32)>` on the method signature itself - an enum
/// keeps the "clear everything" vs "clear one (topic, partition)" choice
/// explicit at call sites.
#[domain_model]
#[derive(Debug, Clone)]
pub enum ProducerResetScope {
    All,
    TopicPartition { topic: String, partition: i32 },
}

#[async_trait]
pub trait IngestService: Send + Sync {
    /// Validate, sequence, and enqueue one event for persistence
    /// (`DESIGN.md:638`).
    async fn publish_event(
        &self,
        ctx: &SecurityContext,
        request: PublishRequest,
    ) -> Result<Event, DomainError>;

    /// Validate and enqueue a batch of events whose event types all resolve to
    /// the same topic (`DESIGN.md:639`). Events name no topic themselves, so
    /// batch homogeneity is a property of the resolved types, not of a field
    /// the producer set.
    async fn publish_batch(
        &self,
        ctx: &SecurityContext,
        requests: Vec<PublishRequest>,
    ) -> Result<BatchResult, DomainError>;

    /// Mint a fresh `producer_id` bound to `ctx`'s calling principal
    /// (`docs/openapi.yaml`'s `POST /v1/producers`).
    async fn register_producer(
        &self,
        ctx: &SecurityContext,
        input: ProducerRegistrationInput,
    ) -> Result<ProducerRegistration, DomainError>;

    /// Read per-`(topic, partition)` `last_sequence` cursors for a producer.
    /// Principal-bound - only the registering principal may call this
    /// (`docs/openapi.yaml`'s `GET /v1/producers/{id}/cursors`).
    async fn get_producer_cursors(
        &self,
        ctx: &SecurityContext,
        producer_id: Uuid,
    ) -> Result<ProducerCursors, DomainError>;

    /// Operator-driven chain reset. Principal-bound - only the registering
    /// principal may call this (`docs/openapi.yaml`'s
    /// `POST /v1/producers/{id}:reset`).
    async fn reset_producer(
        &self,
        ctx: &SecurityContext,
        producer_id: Uuid,
        scope: ProducerResetScope,
    ) -> Result<(), DomainError>;

    /// `GET /v1/topics` - shared/read-side, forwarded to ingest per the
    /// dispatcher's classification (`eb-dispatcher-routing`, design.md D2).
    async fn list_topics(&self) -> Vec<crate::domain::model::Topic>;

    /// `GET /v1/topics/segments`.
    async fn list_topic_segments(
        &self,
        topic: &GtsInstanceId,
        partition: i32,
    ) -> Result<crate::domain::model::TopicSegmentManifest, DomainError>;

    /// `GET /v1/event-types` - same classification rationale as
    /// `list_topics`.
    async fn list_event_types(&self) -> Vec<EventType>;
}

/// Producer identity/ownership bookkeeping - separate from `EventRepo`'s
/// per-`(producer_id, topic, partition)` sequence state (`IdempotencyGuard`
/// owns that) because registration/ownership/client-agent are keyed purely
/// by `producer_id`, with no topic/partition dimension.
#[async_trait]
pub trait ProducerRegistry: Send + Sync {
    /// `tenant_id` (eb-single-process-implementation decision log entry 28)
    /// is captured from `ctx.subject_tenant_id()` at the call site, never
    /// overridable - matches `ConsumerGroup.tenant_id`'s own "non-overridable,
    /// captured from `SecurityContext`" convention.
    async fn register(
        &self,
        owner: Uuid,
        tenant_id: Uuid,
        mode: ProducerMode,
        client_agent: String,
    ) -> Result<ProducerRegistration, DomainError>;

    /// `Ok(None)` if `producer_id` was never registered. `Err` is now
    /// possible (unlike the in-memory stand-in this replaces) since a real
    /// SQL-backed implementation can fail on infrastructure grounds -
    /// eb-single-process-implementation made every `ProducerRegistry` method
    /// fallible for the same reason every other domain repo trait already
    /// is.
    async fn owner(&self, producer_id: Uuid) -> Result<Option<Uuid>, DomainError>;

    /// `Ok(None)` if `producer_id` was never registered (distinct from a
    /// registered producer with no cursors yet, which returns
    /// `Ok(Some(ProducerCursors { topics: vec![], .. }))`).
    async fn cursors(&self, producer_id: Uuid) -> Result<Option<ProducerCursors>, DomainError>;

    async fn reset(&self, producer_id: Uuid, scope: &ProducerResetScope)
    -> Result<(), DomainError>;
}

/// Real `IngestService`: topic/event-type resolution via
/// `SpecificationManager` (`domain::repo::TopicRepo` no longer exists -
/// eb-single-process-implementation D1), partition resolution
/// (`partition_key` else `tenant_id`, `event-broker-producer-api`'s
/// partition contract), idempotency via `IdempotencyGuard`, and durable
/// append via the backend resolved through `BackendResolver` (`domain::
/// repo::EventRepo` no longer exists either - D3). Generic over one repo
/// type implementing every *remaining* trait it needs (idempotency,
/// producer registry - not topics or events).
pub struct IngestServiceImpl<R> {
    repo: std::sync::Arc<R>,
    policy_enforcer: PolicyEnforcer,
    spec_manager: std::sync::Arc<dyn crate::domain::specification::SpecificationManager>,
    backend_resolver: std::sync::Arc<dyn BackendResolver>,
}

impl<R> IngestServiceImpl<R> {
    /// The topic an event type publishes to, as the type's `topic` trait names
    /// it. An event carries only its type, so this is the only route from a
    /// publish request to a stream.
    ///
    /// # Errors
    /// `DomainError::NotFound` with `EventTypeNotFound` when the type is not
    /// registered.
    async fn resolve_topic(&self, event_type: &GtsTypeId) -> Result<GtsInstanceId, DomainError> {
        self.spec_manager
            .get_event_type(event_type)
            .await
            .map(|resolved| resolved.topic)
            .ok_or_else(|| DomainError::NotFound {
                code: "EventTypeNotFound",
                message: format!("event type '{event_type}' is not registered"),
                resource: event_type.to_string(),
            })
    }

    #[must_use]
    pub fn new(
        repo: std::sync::Arc<R>,
        policy_enforcer: PolicyEnforcer,
        spec_manager: std::sync::Arc<dyn crate::domain::specification::SpecificationManager>,
        backend_resolver: std::sync::Arc<dyn BackendResolver>,
    ) -> Self {
        Self {
            repo,
            policy_enforcer,
            spec_manager,
            backend_resolver,
        }
    }
}

#[async_trait]
impl<R> IngestService for IngestServiceImpl<R>
where
    R: crate::domain::idempotency::IdempotencyGuard + ProducerRegistry + Send + Sync + 'static,
{
    async fn publish_event(
        &self,
        ctx: &SecurityContext,
        request: PublishRequest,
    ) -> Result<Event, DomainError> {
        use crate::domain::idempotency::ProducerIdempotencyOutcome;

        // Authz/tenant-scope enforcement (`gears-rust#4516`,
        // `eb-authz-enforcement`) - before topic/schema validation, per
        // `DESIGN.md`'s Validation Pipeline step 1 ordering.
        self.policy_enforcer
            .access_scope_with(
                ctx,
                &EVENT_TYPE_RESOURCE,
                "produce",
                None,
                &AccessRequest::new()
                    .resource_property("event_type_id", request.r#type.as_ref())
                    .require_constraints(false),
            )
            .await
            .map_err(|e| {
                crate::domain::authz::with_forbidden_code(e.into(), "NotAuthorizedToProduce")
            })?;
        tenant_authorized(ctx, &self.policy_enforcer, "produce", request.tenant_id).await?;

        // [todo]: usually event has few first-level fields that can differ depending on the type (overridden); most of the first-level fields can be checked deterministically (performance);
        let event_type = self
            .spec_manager
            .get_event_type(&request.r#type)
            .await
            .ok_or_else(|| DomainError::NotFound {
                code: "EventTypeNotFound",
                message: format!("event type '{}' is not registered", request.r#type),
                resource: request.r#type.to_string(),
            })?;

        // The event type names the stream, so the type lookup above is also
        // the topic lookup: nothing the producer sent can point elsewhere.
        let topic = self
            .spec_manager
            .get_topic(&event_type.topic)
            .await
            .ok_or_else(|| DomainError::NotFound {
                code: "TopicNotFound",
                message: format!("topic '{}' is not registered", event_type.topic),
                resource: event_type.topic.to_string(),
            })?;

        subject_type_allowed(&event_type, &request.subject_type)?;

        self.spec_manager
            .validate_event_data(&event_type, &request.data)
            .await?;

        // The routing contract is the event type's, not this publisher's: the
        // type names a member of the event with a JSON Pointer, and the value
        // at it is what decides the partition. Every producer of the type
        // therefore routes identically, which is the property consumers of it
        // depend on.
        let partition_input =
            crate::domain::backend::partition_input(&request, &event_type.partition_key)?;
        let partition = partition_for(
            &partition_input,
            (*topic.settings.partitions().value())
                .max(1)
                .cast_unsigned(),
        );

        // Broker-logical `sequence` is genuinely unknowable here: persist
        // is now asynchronous (design.md D5's ingest-side outbox), matching
        // DESIGN.md's "the producer gets 202 in milliseconds... backend
        // persist completes asynchronously" - `sequence`/`sequence_time`
        // stay `None` on the returned `Event` rather than a wasteful
        // read-after-write for a value this design doesn't have yet.
        // The resolved topic and the selected `partition` are stamped here so
        // the outbox payload (this exact `Event`, JSON-encoded - no separate
        // envelope type) carries both through to the leased handler.
        let event = Event {
            id: request.id,
            r#type: request.r#type,
            topic: topic.id.clone(),
            tenant_id: request.tenant_id,
            source: request.source,
            subject: request.subject,
            subject_type: request.subject_type,
            occurred_at: request.occurred_at,
            trace_parent: request.trace_parent,
            data: request.data,
            meta: request.meta,
            partition: Some(partition),
            sequence: None,
            sequence_time: None,
        };
        let payload = serde_json::to_vec(&event)
            .map_err(|e| DomainError::Internal(format!("serialize ingest outbox payload: {e}")))?;

        let mut request = crate::domain::idempotency::PublishEnqueue::builder(
            partition,
            payload,
            crate::domain::outbox::INGEST_PAYLOAD_TYPE,
        );
        if let Some(meta) = event.meta.clone() {
            request = request.chain(crate::domain::idempotency::ProducerChainCheck {
                producer_id: meta.producer_id,
                topic: topic.id.clone(),
                partition,
                previous: meta.previous,
                sequence: meta.sequence,
            });
        }

        // `check_and_enqueue` is the one DB transaction spanning the
        // producer-chain check and the outbox insert (design.md D5;
        // `domain/idempotency.rs`'s trait doc) - the actual backend persist
        // happens later, out-of-transaction, when the outbox processor
        // drains the row.
        match self.repo.check_and_enqueue(request.build()).await? {
            ProducerIdempotencyOutcome::Accept => {}
            ProducerIdempotencyOutcome::DuplicateIgnore => return Ok(event),
            ProducerIdempotencyOutcome::SequenceViolation { last_sequence } => {
                return Err(DomainError::SequenceViolation {
                    topic: topic.id.to_string(),
                    partition,
                    last_sequence,
                });
            }
        }

        Ok(event)
    }

    #[toolkit_macros::temporary(
        tracking = "gears-rust#4347",
        reason = "loops over `publish_event` per event instead of one \
                  atomic multi-event append/idempotency-check - no shared \
                  transaction spans the batch (see the comment above); must \
                  become real batch-level append/check-and-record once a \
                  transactional backend or outbox lands"
    )]
    async fn publish_batch(
        &self,
        ctx: &SecurityContext,
        requests: Vec<PublishRequest>,
    ) -> Result<BatchResult, DomainError> {
        const MAX_BATCH: usize = 100;
        if requests.len() > MAX_BATCH {
            return Err(DomainError::BatchTooLarge {
                count: requests.len(),
                max: MAX_BATCH,
            });
        }
        if requests.is_empty() {
            return Ok(BatchResult::default());
        }

        // A batch is atomic per topic, and a request names only its event
        // type, so homogeneity is decided by resolving each type to the topic
        // its `topic` trait names and comparing those. Two requests naming the
        // same topic through different types are one batch; two naming
        // different topics are rejected whatever else they have in common.
        let mut batch_topic: Option<GtsInstanceId> = None;
        for request in &requests {
            let topic = self.resolve_topic(&request.r#type).await?;
            match &batch_topic {
                None => batch_topic = Some(topic),
                Some(first) if *first == topic => {}
                Some(_) => {
                    return Err(DomainError::Validation {
                        code: "MixedTopics",
                        message: "a batch must target a single topic".to_owned(),
                    });
                }
            }
        }

        // Per-event durability is not rolled back on a mid-batch
        // `SequenceViolation` (documented simplification of this in-memory
        // backing - see `eb-rest-handlers`'s design.md; a real transactional
        // backend would wrap the whole batch in one transaction instead).
        let mut result = BatchResult::default();
        for request in requests {
            let id = request.id;
            match self.publish_event(ctx, request).await {
                Ok(stamped) => result.accepted.push(stamped.id),
                Err(err @ DomainError::SequenceViolation { .. }) => return Err(err),
                Err(err) => result.failed.push((id, err.to_string())),
            }
        }
        Ok(result)
    }

    async fn register_producer(
        &self,
        ctx: &SecurityContext,
        input: ProducerRegistrationInput,
    ) -> Result<ProducerRegistration, DomainError> {
        self.repo
            .register(
                ctx.subject_id(),
                ctx.subject_tenant_id(),
                input.mode,
                input.client_agent,
            )
            .await
    }

    async fn get_producer_cursors(
        &self,
        ctx: &SecurityContext,
        producer_id: Uuid,
    ) -> Result<ProducerCursors, DomainError> {
        let owner = self
            .repo
            .owner(producer_id)
            .await?
            .ok_or_else(|| DomainError::NotFound {
                code: "ProducerNotFound",
                message: format!("producer '{producer_id}' is not registered"),
                resource: producer_id.to_string(),
            })?;
        if owner != ctx.subject_id() {
            return Err(DomainError::Forbidden {
                code: "ProducerNotOwned",
                message: "calling principal does not own this producer_id".to_owned(),
                resource: producer_id.to_string(),
            });
        }
        self.repo
            .cursors(producer_id)
            .await?
            .ok_or_else(|| DomainError::NotFound {
                code: "ProducerNotFound",
                message: format!("producer '{producer_id}' is not registered"),
                resource: producer_id.to_string(),
            })
    }

    async fn reset_producer(
        &self,
        ctx: &SecurityContext,
        producer_id: Uuid,
        scope: ProducerResetScope,
    ) -> Result<(), DomainError> {
        let owner = self
            .repo
            .owner(producer_id)
            .await?
            .ok_or_else(|| DomainError::NotFound {
                code: "ProducerNotFound",
                message: format!("producer '{producer_id}' is not registered"),
                resource: producer_id.to_string(),
            })?;
        if owner != ctx.subject_id() {
            return Err(DomainError::Forbidden {
                code: "ProducerNotOwned",
                message: "calling principal does not own this producer_id".to_owned(),
                resource: producer_id.to_string(),
            });
        }
        self.repo.reset(producer_id, &scope).await
    }

    async fn list_topics(&self) -> Vec<crate::domain::model::Topic> {
        self.spec_manager.list_topics().await
    }

    async fn list_topic_segments(
        &self,
        topic_id: &GtsInstanceId,
        partition: i32,
    ) -> Result<crate::domain::model::TopicSegmentManifest, DomainError> {
        let topic =
            self.spec_manager
                .get_topic(topic_id)
                .await
                .ok_or_else(|| DomainError::NotFound {
                    code: "TopicNotFound",
                    message: format!("topic '{topic_id}' is not registered"),
                    resource: topic_id.to_string(),
                })?;
        let backend = self.backend_resolver.resolve(&topic);
        // `IngestService::list_topic_segments` doesn't thread a
        // `SecurityContext` through (a pre-existing gap, not introduced
        // here) - harmless for this backend's `query`, which doesn't use
        // `ctx` for anything (DESIGN.md: "the backend knows nothing about...
        // it is pure storage").
        let segments = backend
            .query(
                &SecurityContext::anonymous(),
                &topic.id.to_string(),
                partition.cast_unsigned(),
                event_broker_sdk::models::PartitionRange {
                    start_offset: None,
                    end_offset: None,
                    limit: u32::MAX,
                },
            )
            .await?;
        let Some(segment) = segments.into_iter().next() else {
            return Ok(crate::domain::model::TopicSegmentManifest {
                topic: topic.id,
                partition,
                start_sequence: Sequence::assigned(0),
                end_sequence: Sequence::assigned(0),
                start_time: None,
                end_time: None,
                segments: vec![serde_json::json!({
                    "start_sequence": 0,
                    "end_sequence": 0,
                    "event_count": 0,
                })],
            });
        };
        Ok(crate::domain::model::TopicSegmentManifest {
            topic: topic.id,
            partition,
            start_sequence: segment.start_sequence,
            end_sequence: segment.end_sequence,
            start_time: segment.start_time,
            end_time: segment.end_time,
            segments: segment.segments,
        })
    }

    async fn list_event_types(&self) -> Vec<EventType> {
        self.spec_manager.list_event_types().await
    }
}

/// The partition a resolved key belongs to.
///
/// Murmur3 (32-bit, x86 variant) with a zero seed, masked to strip the sign bit
/// so the modulo operates on a non-negative value, then reduced by the topic's
/// partition count. The mask is what keeps a signed remainder out of the
/// answer. Hashed the same way the SDK does
/// (`event-broker-sdk/src/mock/partitioning.rs`), so a producer computing a
/// local hint and the broker deciding authoritatively cannot disagree.
fn partition_for(key: &str, partition_count: u32) -> i32 {
    let count = partition_count.max(1);
    ((toolkit_stable_hash::murmur3_x86_32(key.as_bytes(), 0) & 0x7FFF_FFFF) % count).cast_signed()
}

/// `event_type.allowed_subject_types` pattern grammar (`DESIGN.md` §3.1) via
/// `gts::GtsId::matches_pattern` - real, segment-aware GTS pattern matching,
/// not a hand-rolled string check. `event.subject_type` is a *type*
/// reference (a `*_type`-named field per the GTS spec's field-naming
/// convention - "the KIND of entity", not one instance of it), so it's
/// validated as a GTS Type id (trailing `~`), not a bare instance id.
/// Callers can trust every entry in `allowed_subject_types` is already a
/// valid `GtsIdPattern` (`domain::specification::validate_allowed_subject_types`
/// enforces this at registration), so a parse failure here is
/// defensive-only, not the primary enforcement mechanism - a still-malformed
/// entry (e.g. data written before that validation shipped) is skipped
/// rather than aborting the whole check.
fn subject_type_allowed(event_type: &EventType, subject_type: &str) -> Result<(), DomainError> {
    let candidate = GtsId::try_new(subject_type).map_err(|err| DomainError::Validation {
        code: "InvalidSubjectType",
        message: format!("'{subject_type}' is not a valid GTS id: {err}"),
    })?;
    if !candidate.is_type() {
        return Err(DomainError::Validation {
            code: "InvalidSubjectType",
            message: format!(
                "'{subject_type}' must be a GTS type id (trailing '~') - subject_type names a \
                 kind of entity, not one instance of it"
            ),
        });
    }
    let allowed = event_type
        .allowed_subject_types
        .iter()
        .filter_map(|pattern| GtsIdPattern::try_new(pattern).ok())
        .any(|pattern| candidate.matches_pattern(&pattern));
    if allowed {
        Ok(())
    } else {
        Err(DomainError::Validation {
            code: "SubjectTypeNotAllowed",
            message: format!(
                "subject_type '{subject_type}' is not in event type '{}''s allowed_subject_types",
                event_type.id
            ),
        })
    }
}

#[cfg(test)]
#[path = "ingest_tests.rs"]
mod ingest_tests;
