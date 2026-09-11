//! `DeliveryService` (`DESIGN.md:641-658`): consumer-facing, owns the read
//! path and subscription lifecycle.
//!
//! Streaming here is the baseline subset of `event-broker-stream-lifecycle`
//! only: open-time `topology` frame, `event`/`heartbeat` frames,
//! `StreamingInProgress`/`PositionsNotSet` validation, and
//! `DELETE`-as-priority-interrupt. Rebalance-triggered mid-stream frames
//! (`topology` on loss, `terminal` on gain/lose-all, `410
//! SubscriptionTerminated`) need a consumer-group rebalance coordinator that
//! doesn't exist yet and are deferred to a follow-up `OpenSpec` change (see
//! `eb-rest-handlers`'s design.md "Streaming/rebalance scope").
//!
//! The event-delivery loop wakes on `domain::notify::DeliveryNotifier`
//! (design.md D6: a `ClusterCacheV1`-backed, payload-free notification the
//! ingest outbox publishes after a successful persist - see that module's
//! own doc comment for the exact mechanism and a correction against D6's
//! literal per-`(topic, partition)`-key wording) rather than a busy-poll.
//! Still bounded by `heartbeat_interval` each iteration, so idle-heartbeat
//! cadence is unaffected by whether a notification ever arrives.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use authz_resolver_sdk::{AccessRequest, PolicyEnforcer};
use chrono::Utc;
use toolkit::domain_model;
use toolkit_gts::GtsInstanceId;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::authz::{
    CONSUMER_GROUP_RESOURCE, EVENT_TYPE_RESOURCE, TOPIC_RESOURCE, tenant_authorized,
    with_forbidden_code,
};
use crate::domain::backend::BackendResolver;
use crate::domain::consumer_group_coordinator::{ConsumerGroupCoordinator, TopicInterest};
pub use event_broker_sdk::api::Position;

use crate::domain::error::DomainError;
use crate::domain::model::{Interest, Sequence, Subscription};
use crate::domain::notify::DeliveryNotifier;
use crate::domain::streaming::filter::{EventFilter, InterestFilter};
use crate::domain::streaming::lease::StreamLeases;
use crate::domain::streaming::progress::ProgressConfig;
use crate::domain::streaming::read::{MaxBytes, MaxEvents, ReadLimit};
use crate::domain::streaming::read_set::ReadSet;
use crate::domain::streaming::session::{FrameStream, SessionOpening, StreamSession};

/// JOIN request body (`DESIGN.md:692`: `consumer_group` + `interests[]`).
/// `session_timeout` is always concrete here - the REST layer resolves the
/// documented `PT30S` default before constructing this.
#[domain_model]
#[derive(Debug, Clone)]
pub struct JoinRequest {
    pub consumer_group: GtsInstanceId,
    pub client_agent: String,
    pub interests: Vec<Interest>,
    pub session_timeout: Duration,
}

/// A SEEK request value (`event-broker-seek-endpoint-shape`): an exact
/// cursor, or a sentinel the broker resolves against the partition's retained
/// events.
/// One SEEK request target. A subscription can span multiple topics, and
/// assignments/cursors are identified by the full `(topic, partition)`
/// pair (`DESIGN.md:658`) - keying by `partition` alone would collapse
/// partition `0` of two different topics into one entry.
#[domain_model]
#[derive(Debug, Clone)]
pub struct SeekTarget {
    pub topic: GtsInstanceId,
    pub partition: i32,
    pub value: Position,
}

/// One resolved SEEK result - sentinels are already resolved to a concrete
/// cursor (`event-broker-seek-endpoint-shape`'s response requirement).
#[domain_model]
#[derive(Debug, Clone)]
pub struct SeekPosition {
    pub topic: GtsInstanceId,
    pub partition: i32,
    pub offset: Sequence,
}

/// `control` frame fact (`event-broker-consumption-frames`) - the reason
/// (`rebalanced`/`lose_all`/`teardown`) rides in `Frame::Control::reason`,
/// not the code, so recovery logic switches on the fact.
// `Frame`, `ControlCode`, `Position` and `CloseReason` live in
// `domain/streaming/frames.rs` - the session is their sole constructor, so they
// belong with it rather than with the service that used to build them inline.
pub use crate::domain::streaming::frames::{ControlCode, Frame};

/// Stream exclusion lives on `domain::streaming::lease::StreamLeases`, not
/// here. An `ActiveStreamMarker` trait plus a `StreamHandle` carrying a
/// `ClearStreamingOnDrop` guard used to do this job: two lifetimes for one
/// fact, kept in step by a guard whose necessity needed a paragraph to
/// explain. The lease is a field of the session the returned stream owns, so
/// dropping the stream releases it by construction - including before the
/// stream is ever polled, which is what
/// `event-broker-stream-lifecycle` requires.

#[async_trait]
pub trait DeliveryService: Send + Sync {
    /// Creates a subscription in cache, claims/joins the group
    /// (`DESIGN.md:655`).
    async fn join(
        &self,
        ctx: &SecurityContext,
        request: JoinRequest,
    ) -> Result<Subscription, DomainError>;

    /// Removes the subscription from its group (`DESIGN.md:656`). No
    /// rebalance signal to other members this pass - see module doc
    /// comment.
    async fn leave(&self, ctx: &SecurityContext, subscription_id: Uuid) -> Result<(), DomainError>;

    /// Unfiltered/unpaginated - the REST handler applies `$filter`/pagination
    /// (`api/rest/pagination.rs`).
    async fn list_subscriptions(
        &self,
        ctx: &SecurityContext,
    ) -> Result<Vec<Subscription>, DomainError>;

    async fn get_subscription(
        &self,
        ctx: &SecurityContext,
        subscription_id: Uuid,
    ) -> Result<Subscription, DomainError>;

    /// Opens the consumption stream for `subscription_id`: validates
    /// `StreamingInProgress`/`PositionsNotSet`, then returns a channel whose
    /// first message is the open-time `topology` baseline frame, followed
    /// by `event`/`heartbeat` frames. Dropping the receiver clears the
    /// subscription's active-stream marker (`event-broker-stream-lifecycle`).
    async fn stream(
        &self,
        ctx: &SecurityContext,
        subscription_id: Uuid,
    ) -> Result<FrameStream, DomainError>;

    /// Resolves and sets the cursor position for each target
    /// `(topic, partition)` (`DESIGN.md:658`,
    /// `event-broker-seek-endpoint-shape`). Pre-stream-only: rejects with
    /// `Conflict { code: "StreamingInProgress", .. }` while a stream is
    /// open, without disturbing it.
    async fn seek(
        &self,
        ctx: &SecurityContext,
        subscription_id: Uuid,
        targets: Vec<SeekTarget>,
    ) -> Result<Vec<SeekPosition>, DomainError>;

    /// Mints a fresh anonymous consumer group
    /// (`gts.cf.core.events.consumer_group.v1~<uuid>`,
    /// `docs/openapi.yaml`'s `POST /v1/consumer-groups` - "`kind` is always
    /// `anonymous` for this endpoint"; named groups come from
    /// `types_registry`, not this call).
    async fn create_consumer_group(
        &self,
        ctx: &SecurityContext,
        input: crate::domain::model::ConsumerGroupCreateInput,
    ) -> Result<crate::domain::model::ConsumerGroup, DomainError>;

    async fn get_consumer_group(
        &self,
        ctx: &SecurityContext,
        id: &GtsInstanceId,
    ) -> Result<crate::domain::model::ConsumerGroup, DomainError>;

    /// Unfiltered/unpaginated beyond tenant scoping - the REST handler
    /// applies `$filter`/pagination (`api/rest/pagination.rs`), matching
    /// `list_topics`/`list_event_types`. `Anonymous` groups are filtered to
    /// `ctx`'s authorized tenant(s); `Named` groups are returned unfiltered
    /// (`eb-tenant-isolation-fix`).
    async fn list_consumer_groups(
        &self,
        ctx: &SecurityContext,
    ) -> Result<Vec<crate::domain::model::ConsumerGroup>, DomainError>;

    /// Fails with `Conflict { code: "ConsumerGroupHasActiveMembers", .. }`
    /// if any subscription currently belongs to the group
    /// (`docs/openapi.yaml`: "Allowed only when there are no active
    /// members").
    async fn delete_consumer_group(
        &self,
        ctx: &SecurityContext,
        id: &GtsInstanceId,
    ) -> Result<(), DomainError>;
}

/// How long a partition may stay unaccounted-for before the stream is closed
/// rather than heartbeating forever over data it cannot read.
///
/// A constant rather than a knob: `event-broker-stream-pipeline` calls it
/// configurable, but adding a knob is a config change and this is the value the
/// session tests already use. Promoting it belongs with the rest of group 6.
const UNANSWERABLE_TOLERANCE: Duration = Duration::from_secs(30);

/// Real `DeliveryService`: subscription lifecycle plus the baseline
/// (non-rebalance) consumption stream. Generic over one repo type
/// implementing every remaining trait it needs (subscriptions, cursors,
/// consumer groups - not topics or events, per `IngestServiceImpl`'s same
/// D1/D3 change), plus `SpecificationManager`/`BackendResolver` for topic
/// resolution and event reads.
pub struct DeliveryServiceImpl<R> {
    repo: Arc<R>,
    heartbeat_interval: Duration,
    policy_enforcer: PolicyEnforcer,
    spec_manager: Arc<dyn crate::domain::specification::SpecificationManager>,
    backend_resolver: Arc<dyn BackendResolver>,
    groups: Arc<ConsumerGroupCoordinator>,
    /// Read batch bounds and progress cadence, from configuration rather than
    /// from literals at the call site.
    streaming: crate::config::StreamingConfig,
    /// The partition caches a session attaches readers to.
    topics: Arc<crate::infra::loader::topics::TopicManager>,
    /// Stream exclusion, moving here from `Storage`'s active-stream marker.
    leases: Arc<crate::domain::streaming::lease::InProcessStreamLeases>,
}

impl<R> DeliveryServiceImpl<R> {
    #[must_use]
    pub fn new(
        repo: Arc<R>,
        policy_enforcer: PolicyEnforcer,
        spec_manager: Arc<dyn crate::domain::specification::SpecificationManager>,
        backend_resolver: Arc<dyn BackendResolver>,
        groups: Arc<ConsumerGroupCoordinator>,
        topics: Arc<crate::infra::loader::topics::TopicManager>,
        leases: Arc<crate::domain::streaming::lease::InProcessStreamLeases>,
        streaming: crate::config::StreamingConfig,
    ) -> Self {
        Self {
            repo,
            heartbeat_interval: Duration::from_secs(u64::from(streaming.heartbeat_interval_secs)),
            policy_enforcer,
            spec_manager,
            backend_resolver,
            groups,
            streaming,
            topics,
            leases,
        }
    }
}

impl<R> DeliveryServiceImpl<R>
where
    R: crate::domain::repo::SubscriptionRepo + Send + Sync + 'static,
{
    /// Fetches `subscription_id`, returning `SubscriptionNotFound` if
    /// absent, then verifies the calling principal is authorized to act as
    /// the subscription's `tenant_id` (`domain::authz::tenant_authorized`,
    /// the same check `join` applies when creating it) - a denial surfaces
    /// as `Forbidden { code: "TenantIdNotAuthorized", .. }` before the
    /// caller reads or mutates anything about the subscription
    /// (`eb-tenant-isolation-fix`).
    async fn find_authorized_subscription(
        &self,
        ctx: &SecurityContext,
        subscription_id: Uuid,
    ) -> Result<Subscription, DomainError> {
        let subscription = self
            .repo
            .find_subscription(subscription_id)
            .await?
            .ok_or_else(|| DomainError::NotFound {
                code: "SubscriptionNotFound",
                message: format!("subscription '{subscription_id}' does not exist"),
                resource: subscription_id.to_string(),
            })?;
        tenant_authorized(
            ctx,
            &self.policy_enforcer,
            "consume",
            subscription.tenant_id,
        )
        .await?;
        Ok(subscription)
    }
}

/// Filters `items` down to the ones whose tenant (`tenant_of`) the calling
/// principal is authorized for, calling `tenant_authorized` once per
/// *distinct* tenant present rather than once per item
/// (`eb-tenant-isolation-fix`'s "list filtering" design decision) - a
/// denial excludes that tenant's items; any other error propagates, since
/// that's a genuine backend failure, not a decline.
async fn filter_by_authorized_tenant<T>(
    items: Vec<T>,
    ctx: &SecurityContext,
    policy_enforcer: &PolicyEnforcer,
    action: &'static str,
    tenant_of: impl Fn(&T) -> Uuid,
) -> Result<Vec<T>, DomainError> {
    let mut authorized: std::collections::HashMap<Uuid, bool> = std::collections::HashMap::new();
    let mut result = Vec::with_capacity(items.len());
    for item in items {
        let tenant = tenant_of(&item);
        let is_authorized = if let Some(cached) = authorized.get(&tenant) {
            *cached
        } else {
            let ok = match tenant_authorized(ctx, policy_enforcer, action, tenant).await {
                Ok(()) => true,
                Err(DomainError::Forbidden { .. }) => false,
                Err(other) => return Err(other),
            };
            authorized.insert(tenant, ok);
            ok
        };
        if is_authorized {
            result.push(item);
        }
    }
    Ok(result)
}

impl<R> DeliveryServiceImpl<R>
where
    R: crate::domain::repo::ConsumerGroupRepo + Send + Sync + 'static,
{
    /// Fetches `id`, returning `ConsumerGroupNotFound` if absent, then
    /// authorizes `action` against it per `docs/DESIGN.md`'s Consumer Group
    /// Lifecycle shape split: `Anonymous` groups check owner-tenant
    /// equality (`tenant_authorized`, the same rule `join` applies);
    /// `Named` groups check the `action` permission via PEP
    /// (`named_group_authorized`) - they're permission-owned, not
    /// tenant-owned (`eb-tenant-isolation-fix`).
    async fn find_authorized_consumer_group(
        &self,
        ctx: &SecurityContext,
        action: &'static str,
        id: &GtsInstanceId,
    ) -> Result<crate::domain::model::ConsumerGroup, DomainError> {
        let group =
            self.repo
                .find_consumer_group(id)
                .await?
                .ok_or_else(|| DomainError::NotFound {
                    code: "ConsumerGroupNotFound",
                    message: format!("consumer group '{id}' is not registered"),
                    resource: id.to_string(),
                })?;
        match group.kind {
            crate::domain::model::ConsumerGroupKind::Anonymous => {
                tenant_authorized(ctx, &self.policy_enforcer, action, group.tenant_id).await?;
            }
            crate::domain::model::ConsumerGroupKind::Named => {
                self.named_group_authorized(ctx, action, &group.id).await?;
            }
        }
        Ok(group)
    }

    /// Checks `action` (`"define"`/`"consume"`/`"manage"`) against
    /// `CONSUMER_GROUP_RESOURCE` for `group_id` via PEP - the permission
    /// half of the Consumer Group Lifecycle authorization split (`Named`
    /// groups; `Anonymous` groups use `tenant_authorized` instead).
    ///
    /// # Errors
    /// Returns `DomainError::Forbidden { code: "ConsumerGroupNotAuthorized",
    /// .. }` on denial, or the mapped `DomainError` for any other PEP
    /// failure.
    async fn named_group_authorized(
        &self,
        ctx: &SecurityContext,
        action: &'static str,
        group_id: &GtsInstanceId,
    ) -> Result<(), DomainError> {
        self.policy_enforcer
            .access_scope_with(
                ctx,
                &CONSUMER_GROUP_RESOURCE,
                action,
                None,
                &AccessRequest::new()
                    .resource_property("consumer_group_id", group_id.as_ref())
                    .require_constraints(false),
            )
            .await
            .map_err(|e| with_forbidden_code(e.into(), "ConsumerGroupNotAuthorized"))?;
        Ok(())
    }
}

#[async_trait]
impl<R> DeliveryService for DeliveryServiceImpl<R>
where
    R: crate::domain::repo::SubscriptionRepo
        + crate::domain::repo::CursorRepo
        + crate::domain::repo::ConsumerGroupRepo
        + DeliveryNotifier
        + Send
        + Sync
        + 'static,
{
    async fn join(
        &self,
        ctx: &SecurityContext,
        request: JoinRequest,
    ) -> Result<Subscription, DomainError> {
        let group = self
            .repo
            .find_consumer_group(&request.consumer_group)
            .await?
            .ok_or_else(|| DomainError::NotFound {
                code: "ConsumerGroupNotFound",
                resource: request.consumer_group.to_string(),
                message: format!(
                    "consumer group '{}' is not registered - anonymous groups must be created \
                     via POST /v1/consumer-groups first",
                    request.consumer_group
                ),
            })?;
        // Group-level JOIN authorization (`docs/DESIGN.md`'s Consumer Group
        // Lifecycle: "JOIN authorization differs by shape: anonymous →
        // owner-tenant equality; named → explicit :consume permission via
        // PEP") - distinct from the per-interest topic/event-type/tenant
        // checks below, which gate what the caller may *consume*, not
        // whether it may join *this group* at all (`eb-tenant-isolation-fix`).
        match group.kind {
            crate::domain::model::ConsumerGroupKind::Anonymous => {
                tenant_authorized(ctx, &self.policy_enforcer, "consume", group.tenant_id).await?;
            }
            crate::domain::model::ConsumerGroupKind::Named => {
                self.named_group_authorized(ctx, "consume", &group.id)
                    .await?;
            }
        }

        let sub_id = Uuid::new_v4();
        let mut topic_interests = Vec::new();
        let mut topics = Vec::new();
        for interest in &request.interests {
            // Authz/tenant-scope enforcement (`gears-rust#4516`,
            // `eb-authz-enforcement`) - any single denial aborts the whole
            // JOIN via `?`'s early return, before a subscription is
            // created (spec.md "A single unauthorized interest blocks the
            // whole JOIN").
            self.policy_enforcer
                .access_scope_with(
                    ctx,
                    &TOPIC_RESOURCE,
                    "consume",
                    None,
                    &AccessRequest::new()
                        .resource_property("topic_id", interest.topic.as_ref())
                        .require_constraints(false),
                )
                .await
                .map_err(|e| with_forbidden_code(e.into(), "TopicNotAuthorized"))?;

            for pattern in &interest.types {
                validate_type_pattern(pattern)?;
                self.policy_enforcer
                    .access_scope_with(
                        ctx,
                        &EVENT_TYPE_RESOURCE,
                        "consume",
                        None,
                        &AccessRequest::new()
                            .resource_property("event_type_id", pattern.clone())
                            .require_constraints(false),
                    )
                    .await
                    .map_err(|e| {
                        let mut err = with_forbidden_code(e.into(), "EventTypeNotAuthorized");
                        if let DomainError::Forbidden {
                            message, resource, ..
                        } = &mut err
                        {
                            *message = format!(
                                "calling principal lacks consume on event type '{pattern}'"
                            );
                            resource.clone_from(pattern);
                        }
                        err
                    })?;
            }

            tenant_authorized(ctx, &self.policy_enforcer, "consume", interest.tenant_id).await?;

            let topic = self
                .spec_manager
                .get_topic(&interest.topic)
                .await
                .ok_or_else(|| DomainError::NotFound {
                    code: "TopicNotFound",
                    message: format!("topic '{}' is not registered", interest.topic),
                    resource: interest.topic.to_string(),
                })?;
            topics.push(topic.id.clone());
            topic_interests.push(TopicInterest {
                id: topic.id.clone(),
                partitions: *topic.settings.partitions().value(),
            });
        }

        let (assigned, topology_version, sibling_updates) = self.groups.join(
            &request.consumer_group,
            sub_id,
            &topic_interests,
            request.session_timeout,
        );

        for (sibling_id, sibling_assigned) in sibling_updates {
            if let Ok(Some(mut sub)) = self.repo.find_subscription(sibling_id).await {
                sub.assigned = sibling_assigned;
                sub.topology_version = topology_version;
                let _ = self.repo.put_subscription(&sub).await;
            }
        }

        let now = Utc::now();
        let subscription = Subscription {
            id: sub_id,
            tenant_id: ctx.subject_tenant_id(),
            consumer_group: request.consumer_group,
            client_agent: request.client_agent,
            interests: request.interests,
            topics,
            assigned,
            topology_version,
            session_timeout: request.session_timeout,
            last_seen_at: now,
            expires_at: now
                + chrono::Duration::from_std(request.session_timeout)
                    .unwrap_or(chrono::Duration::seconds(30)),
        };
        self.repo.put_subscription(&subscription).await?;
        Ok(subscription)
    }

    async fn leave(&self, ctx: &SecurityContext, subscription_id: Uuid) -> Result<(), DomainError> {
        let subscription = self
            .find_authorized_subscription(ctx, subscription_id)
            .await?;
        self.repo.delete_subscription(subscription_id).await?;
        self.groups
            .leave(&subscription.consumer_group, subscription_id);
        Ok(())
    }

    async fn list_subscriptions(
        &self,
        ctx: &SecurityContext,
    ) -> Result<Vec<Subscription>, DomainError> {
        let subscriptions = self.repo.list_subscriptions().await?;
        filter_by_authorized_tenant(subscriptions, ctx, &self.policy_enforcer, "consume", |s| {
            s.tenant_id
        })
        .await
    }

    async fn get_subscription(
        &self,
        ctx: &SecurityContext,
        subscription_id: Uuid,
    ) -> Result<Subscription, DomainError> {
        self.find_authorized_subscription(ctx, subscription_id)
            .await
    }

    async fn stream(
        &self,
        ctx: &SecurityContext,
        subscription_id: Uuid,
    ) -> Result<FrameStream, DomainError> {
        let subscription = self
            .find_authorized_subscription(ctx, subscription_id)
            .await?;

        // One pass for both jobs: a partition with no cursor is unseeded, and
        // the cursors that do exist are where delivery starts. The previous
        // implementation checked existence here and discarded the values,
        // seeding instead from `Assignment`'s offsets - which the coordinator
        // sets to 0 and never updates.
        let mut unseeded = Vec::new();
        let mut cursors = Vec::with_capacity(subscription.assigned.len());
        for assignment in &subscription.assigned {
            match self
                .repo
                .find_cursor(
                    &subscription.consumer_group,
                    &assignment.topic,
                    assignment.partition,
                )
                .await?
            {
                Some(cursor) => cursors.push(cursor),
                None => unseeded.push(format!("{}:{}", assignment.topic, assignment.partition)),
            }
        }
        if !unseeded.is_empty() {
            return Err(DomainError::Conflict {
                code: "PositionsNotSet",
                message: format!("unseeded assigned partitions: {}", unseeded.join(", ")),
                resource: subscription_id.to_string(),
            });
        }

        // The lease replaces `try_mark_streaming` and its drop guard: it is a
        // field of the session the returned stream owns, so exclusion cannot
        // outlive the stream.
        let lease = self
            .leases
            .acquire(subscription_id)
            .ok_or_else(|| DomainError::Conflict {
                code: "StreamingInProgress",
                message: format!("subscription '{subscription_id}' already has an open stream"),
                resource: subscription_id.to_string(),
            })?;

        // Compiled per open. The previous implementation applied no filter at
        // all, so a subscription received events outside its interests.
        let filter: Arc<dyn EventFilter> =
            Arc::new(InterestFilter::compile(&subscription.interests)?);

        // Subscribing takes the membership handle with it: the session holds
        // both, so dropping the stream both stops reading and reports the
        // stream closed. `None` means the member is gone underneath us, which a
        // caller racing a LEAVE can legitimately see.
        let (generations, membership) =
            crate::domain::consumer_group_coordinator::ConsumerGroupCoordinator::subscribe(
                &self.groups,
                &subscription.consumer_group,
                subscription_id,
            )
            .ok_or_else(|| DomainError::NotFound {
                code: "SubscriptionNotFound",
                message: format!("subscription '{subscription_id}' is no longer a group member"),
                resource: subscription_id.to_string(),
            })?;

        let ready = Arc::new(tokio::sync::Notify::new());
        let slots = crate::infra::loader::attach::attach_readers(
            &crate::infra::loader::attach::AttachRequest {
                topics: &self.topics,
                assigned: &subscription.assigned,
                cursors: &cursors,
                ready: &ready,
            },
        );

        let cfg = &self.streaming;
        let session = StreamSession::open(SessionOpening {
            read_set: ReadSet::seed(slots),
            filter,
            progress: ProgressConfig {
                drift_threshold: cfg.progress_drift_threshold,
                min_interval: Duration::from_secs(u64::from(cfg.progress_min_interval_secs)),
            },
            heartbeat_interval: self.heartbeat_interval,
            limit: ReadLimit::new(
                MaxEvents(cfg.read_batch_max_events),
                MaxBytes(cfg.read_batch_max_bytes),
            ),
            topology_version: subscription.topology_version,
            ready,
            started_at: tokio::time::Instant::now(),
            now: Arc::new(chrono::Utc::now),
            unanswerable_tolerance: UNANSWERABLE_TOLERANCE,
            lease,
            generations,
            membership,
        });

        Ok(FrameStream::new(session))
    }

    async fn seek(
        &self,
        ctx: &SecurityContext,
        subscription_id: Uuid,
        targets: Vec<SeekTarget>,
    ) -> Result<Vec<SeekPosition>, DomainError> {
        let subscription = self
            .find_authorized_subscription(ctx, subscription_id)
            .await?;

        if self.leases.is_held(subscription_id) {
            return Err(DomainError::Conflict {
                code: "StreamingInProgress",
                message: format!("subscription '{subscription_id}' has an open stream"),
                resource: subscription_id.to_string(),
            });
        }

        // Resolve every target before applying any: a request naming several
        // partitions applies all of them or none, so a single inadmissible
        // position leaves no cursor written.
        let mut resolved = Vec::new();
        // Every offending entry is reported, not just the first, so a caller
        // correcting several positions needs one round trip rather than one
        // per partition.
        let mut violations = Vec::new();
        for (index, target) in targets.into_iter().enumerate() {
            let topic = self
                .spec_manager
                .get_topic(&target.topic)
                .await
                .ok_or_else(|| DomainError::NotFound {
                    code: "TopicNotFound",
                    message: format!("topic '{}' is not registered", target.topic),
                    resource: target.topic.to_string(),
                })?;
            let backend = self.backend_resolver.resolve(&topic);
            // The backend owns this: it is the only thing that knows where
            // retention has reached, what it has assigned, and when each event
            // occurred. An inadmissible `Exact` fails to resolve.
            match backend
                .resolve(
                    ctx,
                    &topic.id.to_string(),
                    target.partition.cast_unsigned(),
                    target.value,
                )
                .await
            {
                Ok(offset) => resolved.push(SeekPosition {
                    topic: target.topic,
                    partition: target.partition,
                    offset,
                }),
                Err(event_broker_sdk::StorageBackendError::OffsetOutOfRange {
                    floor,
                    ceiling,
                    breached,
                    ..
                }) => violations.push(
                    event_broker_sdk::PositionViolation::builder(index)
                        .floor(floor)
                        .ceiling(ceiling)
                        .breached(breached)
                        .build(),
                ),
                Err(other) => return Err(other.into()),
            }
        }
        if !violations.is_empty() {
            return Err(DomainError::SeekOutOfRange { violations });
        }

        // Only now that every target resolved does anything become durable.
        for position in &resolved {
            self.repo
                .put_cursor(&crate::domain::model::Cursor {
                    topic: position.topic.clone(),
                    consumer_group: subscription.consumer_group.clone(),
                    partition: position.partition,
                    offset: position.offset,
                })
                .await?;
        }
        Ok(resolved)
    }

    #[toolkit_macros::temporary(
        tracking = "gears-rust#4347",
        reason = "id-mint + uniqueness-check + insert is a single \
                  `repo.create_consumer_group(...)` call with no surrounding \
                  transaction; harmless against `InMemoryDomainRepo`'s single \
                  mutex today, but a real backend needs this wrapped in one \
                  transaction once the pluggable StorageBackend lands"
    )]
    async fn create_consumer_group(
        &self,
        ctx: &SecurityContext,
        input: crate::domain::model::ConsumerGroupCreateInput,
    ) -> Result<crate::domain::model::ConsumerGroup, DomainError> {
        // `docs/DESIGN.md`'s Consumer Group Lifecycle: "Errors: 403
        // Forbidden — caller lacks consumer_group:define permission via
        // PEP". No `consumer_group_id` property - the group doesn't exist
        // yet, so this is a general "may this principal define a group at
        // all" capability check, not one scoped to a specific id
        // (`eb-tenant-isolation-fix`).
        self.policy_enforcer
            .access_scope_with(
                ctx,
                &CONSUMER_GROUP_RESOURCE,
                "define",
                None,
                &AccessRequest::new().require_constraints(false),
            )
            .await
            .map_err(|e| with_forbidden_code(e.into(), "ConsumerGroupNotAuthorized"))?;

        tracing::info!(
            client_agent = input.client_agent.as_deref(),
            "consumer group create requested"
        );
        let group = crate::domain::model::ConsumerGroup {
            id: GtsInstanceId::new(
                event_broker_sdk::gts::CONSUMER_GROUP_RESOURCE_TYPE,
                &Uuid::new_v4().to_string(),
            ),
            kind: crate::domain::model::ConsumerGroupKind::Anonymous,
            tenant_id: ctx.subject_tenant_id(),
            owner_principal_id: ctx.subject_id(),
            description: input.description,
            created_at: Utc::now(),
        };
        self.repo.create_consumer_group(group).await
    }

    async fn get_consumer_group(
        &self,
        ctx: &SecurityContext,
        id: &GtsInstanceId,
    ) -> Result<crate::domain::model::ConsumerGroup, DomainError> {
        self.find_authorized_consumer_group(ctx, "consume", id)
            .await
    }

    async fn list_consumer_groups(
        &self,
        ctx: &SecurityContext,
    ) -> Result<Vec<crate::domain::model::ConsumerGroup>, DomainError> {
        let groups = self.repo.list_consumer_groups().await?;
        let (anonymous, named): (Vec<_>, Vec<_>) = groups
            .into_iter()
            .partition(|g| g.kind == crate::domain::model::ConsumerGroupKind::Anonymous);
        let mut visible =
            filter_by_authorized_tenant(anonymous, ctx, &self.policy_enforcer, "consume", |g| {
                g.tenant_id
            })
            .await?;
        // Named groups have no shared tenant to batch on (`docs/DESIGN.md`:
        // "filtered by AccessScope from the PEP ... any named groups the
        // caller has :consume permission on") - one PEP call per group,
        // matching `join`'s own per-interest checks (no batching there
        // either).
        for group in named {
            match self.named_group_authorized(ctx, "consume", &group.id).await {
                Ok(()) => visible.push(group),
                Err(DomainError::Forbidden { .. }) => {}
                Err(other) => return Err(other),
            }
        }
        Ok(visible)
    }

    async fn delete_consumer_group(
        &self,
        ctx: &SecurityContext,
        id: &GtsInstanceId,
    ) -> Result<(), DomainError> {
        self.find_authorized_consumer_group(ctx, "manage", id)
            .await?;
        if self.repo.has_active_members(id).await? {
            return Err(DomainError::Conflict {
                code: "ConsumerGroupHasActiveMembers",
                message: format!("consumer group '{id}' still has active members"),
                resource: id.to_string(),
            });
        }
        self.repo.delete_consumer_group(id).await
    }
}

/// GTS §10 wildcard rules, the subset `docs/openapi.yaml`'s `BadTypePattern`
/// documents: a wildcard segment (`*`) must fill its whole dot-separated
/// segment (not a substring of one, not mid-pattern text), and at most one
/// segment may be a wildcard.
fn validate_type_pattern(pattern: &str) -> Result<(), DomainError> {
    let segments: Vec<&str> = pattern.split('.').collect();
    let wildcard_segments = segments.iter().filter(|s| s.contains('*')).count();
    let bad_segment = segments.iter().any(|s| s.contains('*') && *s != "*");
    if wildcard_segments > 1 || bad_segment {
        return Err(DomainError::Validation {
            code: "BadTypePattern",
            message: format!(
                "'{pattern}' violates GTS wildcard rules - a wildcard must fill its whole \
                 segment and at most one segment may be a wildcard"
            ),
        });
    }
    Ok(())
}
