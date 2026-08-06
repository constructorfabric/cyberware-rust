use async_trait::async_trait;
use secrecy::ExposeSecret;
use serde::de::DeserializeOwned;
use toolkit_canonical_errors::{CanonicalError, Problem};
use toolkit_http::{HttpClient, HttpResponse, RequestBuilder};
use toolkit_security::SecurityContext;

use crate::api::{
    EventBrokerApi, IngestOutcome, JoinRequest, ProducerCursors, ProducerMode, SeekPosition,
    SeekResult, SubscriptionAssignment, TenantTraversalDepth,
};
use crate::error::EventBrokerError;
use crate::ids::{ConsumerGroupId, ProducerId, SubscriptionId};
use crate::models::{
    ConsumerGroup, ConsumerGroupQuery, CreateConsumerGroupRequest, Event, EventType, Page,
    PartitionAssignment, PartitionRange, ResetScope, Subscription, Topic, TopicSegment,
};
use crate::rest::stream;
use crate::rest::wire;
use crate::sequence::Sequence;

const V1: &str = "/event-broker/v1";

/// Wire framing for the delivery stream.
///
/// Both framings carry the same frames (`WireFrame`); they differ only in how a
/// frame is delimited on the wire. Cloud clients use multipart; browser clients
/// use SSE.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StreamTransport {
    /// `multipart/mixed` at `/events:stream` - the default (cloud clients).
    #[default]
    Multipart,
    /// `text/event-stream` at `/events:sse` (browser clients).
    Sse,
}

/// REST-transport implementation of [`EventBrokerApi`](crate::api::EventBrokerApi).
///
/// Speaks the broker's `/event-broker/v1/...` HTTP surface. It satisfies the
/// same trait as the mock and any in-process implementation, so consumers
/// resolving `Arc<dyn EventBrokerApi>` cannot tell which one they hold.
pub struct RestBroker {
    http: HttpClient,
    base_url: String,
    framing: StreamTransport,
}

impl RestBroker {
    /// Builds a client against `base_url` (for example `https://host:8080`),
    /// using the default multipart stream framing.
    ///
    /// # Errors
    /// [`EventBrokerError::Transport`] if the underlying HTTP client cannot be
    /// constructed.
    pub fn new(base_url: impl Into<String>) -> Result<Self, EventBrokerError> {
        let http = HttpClient::new().map_err(|err| EventBrokerError::Transport(err.to_string()))?;
        Ok(Self {
            http,
            base_url: base_url.into(),
            framing: StreamTransport::default(),
        })
    }

    /// Selects the delivery stream framing this client opens streams with.
    #[must_use]
    pub fn with_stream_transport(mut self, framing: StreamTransport) -> Self {
        self.framing = framing;
        self
    }

    pub(crate) fn http(&self) -> &HttpClient {
        &self.http
    }

    pub(crate) fn framing(&self) -> StreamTransport {
        self.framing
    }

    /// Joins the base URL with a `/event-broker/v1`-rooted path.
    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url.trim_end_matches('/'))
    }

    /// Attaches the caller's tenant bearer credential, if present, so the broker
    /// authorizes the request as that tenant. The secret is exposed only here,
    /// at the transport boundary.
    pub(crate) fn authed(&self, rb: RequestBuilder, ctx: &SecurityContext) -> RequestBuilder {
        match ctx.bearer_token() {
            Some(token) => rb.bearer_auth(token.expose_secret()),
            None => rb,
        }
    }
}

fn to_transport(err: toolkit_http::HttpError) -> EventBrokerError {
    EventBrokerError::Transport(err.to_string())
}

/// Maps a non-success response to a typed [`EventBrokerError`] by decoding the
/// RFC 9457 problem body to a [`CanonicalError`] and inverting it. A body that
/// is not a recognizable problem becomes a transport error naming the status.
pub(crate) async fn error_from_response(resp: HttpResponse) -> EventBrokerError {
    let status = resp.status();
    let bytes = resp.bytes().await.unwrap_or_default();
    match serde_json::from_slice::<Problem>(&bytes) {
        Ok(problem) => match CanonicalError::try_from(problem) {
            Ok(canonical) => EventBrokerError::from(canonical),
            Err(_) => {
                EventBrokerError::Transport(format!("broker returned {status} with a problem body"))
            }
        },
        Err(_) => EventBrokerError::Transport(format!(
            "broker returned {status} with an unrecognized body"
        )),
    }
}

impl RestBroker {
    async fn send_json<T: DeserializeOwned>(
        &self,
        rb: RequestBuilder,
    ) -> Result<T, EventBrokerError> {
        let resp = rb.send().await.map_err(to_transport)?;
        if resp.status().is_success() {
            resp.json::<T>().await.map_err(to_transport)
        } else {
            Err(error_from_response(resp).await)
        }
    }

    async fn send_status(&self, rb: RequestBuilder) -> Result<u16, EventBrokerError> {
        let resp = rb.send().await.map_err(to_transport)?;
        if resp.status().is_success() {
            Ok(resp.status().as_u16())
        } else {
            Err(error_from_response(resp).await)
        }
    }

    /// Follows `next_cursor` to accumulate every page of a list endpoint whose
    /// SDK method returns a flat `Vec`.
    async fn list_all<T: DeserializeOwned>(
        &self,
        ctx: &SecurityContext,
        path: &str,
    ) -> Result<Vec<T>, EventBrokerError> {
        let mut items = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let url = match &cursor {
                Some(c) => self.url(&format!("{path}?cursor={c}")),
                None => self.url(path),
            };
            let rb = self.authed(self.http().get(&url), ctx);
            let page: wire::PageWire<T> = self.send_json(rb).await?;
            items.extend(page.items);
            match page.page_info.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(items)
    }
}

/// Builds one interest wire body from a public [`SubscriptionInterest`].
fn interest_to_wire(interest: &crate::api::SubscriptionInterest) -> wire::InterestWire {
    let max_depth = match interest.tenant_depth() {
        TenantTraversalDepth::CurrentTenant => wire::MaxDepthWire::Levels(0),
        TenantTraversalDepth::Descendants(n) => {
            wire::MaxDepthWire::Levels(i32::try_from(n.get()).unwrap_or(i32::MAX))
        }
        TenantTraversalDepth::UnlimitedDescendants => wire::MaxDepthWire::Unlimited,
    };
    wire::InterestWire {
        topic: interest.topic().to_owned(),
        tenant_id: interest.tenant_id(),
        max_depth,
        barrier_mode: interest.barrier_mode().into(),
        types: interest.types().to_vec(),
        filter: interest.filter().map(|f| wire::FilterSpecWire {
            engine: f.engine().to_owned(),
            expression: f.expression().to_owned(),
        }),
    }
}

/// Parses a group's GTS instance id from a broker response into the SDK's
/// uuid-backed `ConsumerGroupId`. Only anonymous groups (`<type>~<uuid>`) are
/// supported for now; a named group's id has no uuid to carry, so it is reported
/// as a transport-level surprise rather than silently mishandled.
fn consumer_group_id_from_wire(gts: &str) -> Result<ConsumerGroupId, EventBrokerError> {
    ConsumerGroupId::try_from_gts(gts).ok_or_else(|| {
        EventBrokerError::Transport(
            "broker returned a non-anonymous consumer group id; only anonymous groups are supported"
                .to_owned(),
        )
    })
}

/// Builds a public [`ConsumerGroup`] from its wire body.
fn consumer_group_from_wire(w: wire::ConsumerGroupWire) -> Result<ConsumerGroup, EventBrokerError> {
    Ok(ConsumerGroup {
        id: consumer_group_id_from_wire(&w.id)?,
        tenant_id: w.tenant_id,
        owner_principal_id: w.owner_principal_id,
        kind: w.kind.into(),
        description: w.description,
        created_at: w.created_at,
    })
}

/// Builds a public [`Subscription`] from its wire body. The wire also carries
/// `client_agent` and `interests`, which the SDK `Subscription` does not model.
fn subscription_from_wire(w: wire::SubscriptionWire) -> Result<Subscription, EventBrokerError> {
    let assigned = w
        .assigned
        .into_iter()
        .map(|a| {
            Ok(PartitionAssignment {
                topic: gts::GtsInstanceId::try_new(&a.topic).map_err(|err| {
                    EventBrokerError::Transport(format!("subscription carried an invalid topic: {err}"))
                })?,
                partition: u32::try_from(a.partition).unwrap_or_default(),
            })
        })
        .collect::<Result<Vec<_>, EventBrokerError>>()?;
    let interests = w
        .interests
        .into_iter()
        .map(interest_from_wire)
        .collect::<Result<Vec<_>, EventBrokerError>>()?;
    Ok(Subscription {
        id: SubscriptionId(w.id),
        consumer_group: consumer_group_id_from_wire(&w.consumer_group)?,
        client_agent: w.client_agent,
        interests,
        assigned,
        topology_version: w.topology_version,
        expires_at: w.expires_at,
    })
}

/// Builds one public [`SubscriptionInterest`] from its wire echo. `max_depth`
/// carries the tri-state: `null` is unlimited, `0` current-tenant, `n`
/// descendants.
fn interest_from_wire(
    w: wire::InterestResponseWire,
) -> Result<crate::api::SubscriptionInterest, EventBrokerError> {
    let depth = match w.max_depth {
        None => TenantTraversalDepth::UnlimitedDescendants,
        Some(levels) => match u32::try_from(levels).ok().and_then(std::num::NonZeroU32::new) {
            Some(nonzero) => TenantTraversalDepth::descendants(nonzero),
            None => TenantTraversalDepth::CurrentTenant,
        },
    };
    let barrier_mode = match w.barrier_mode.as_str() {
        "ignore" => crate::api::BarrierMode::Ignore,
        _ => crate::api::BarrierMode::Respect,
    };
    let mut builder = crate::api::SubscriptionInterest::builder()
        .topic(w.topic)
        .tenant_id(w.tenant_id)
        .tenant_depth(depth)
        .barrier_mode(barrier_mode)
        .types(w.types);
    if let Some(filter) = w.filter {
        builder = builder.filter(crate::api::Filter::new(filter.engine, filter.expression)?);
    }
    builder.build()
}

/// Builds a public [`EventType`] from its wire body.
fn event_type_from_wire(w: wire::EventTypeWire) -> Result<EventType, EventBrokerError> {
    Ok(EventType {
        id: gts::GtsTypeId::try_new(&w.id).map_err(|err| {
            EventBrokerError::Transport(format!("event type carried an invalid id: {err}"))
        })?,
        topic: gts::GtsInstanceId::try_new(&w.topic).map_err(|err| {
            EventBrokerError::Transport(format!("event type carried an invalid topic: {err}"))
        })?,
        description: w.description,
        allowed_subject_types: w.allowed_subject_types,
        partition_key: w.partition_key,
        data_schema: w.data_schema,
    })
}

/// Builds a public [`Topic`] from its wire body. The wire `description` is
/// optional but the SDK model requires it; an absent one becomes empty.
fn topic_from_wire(w: wire::TopicWire) -> Result<Topic, EventBrokerError> {
    Ok(Topic {
        id: gts::GtsInstanceId::try_new(&w.id).map_err(|err| {
            EventBrokerError::Transport(format!("topic carried an invalid id: {err}"))
        })?,
        description: w.description.unwrap_or_default(),
        retention: w
            .retention
            .and_then(|r| r.parse::<toolkit_utils::iso8601_duration::Iso8601Duration>().ok()),
    })
}

#[async_trait]
impl EventBrokerApi for RestBroker {
    async fn register_producer(
        &self,
        ctx: &SecurityContext,
        mode: ProducerMode,
        client_agent: &str,
    ) -> Result<ProducerId, EventBrokerError> {
        // RECONCILE: the wire register accepts only chained/monotonic; a
        // stateless producer is not registered and has no id to return.
        let mode = match mode {
            ProducerMode::Chained => wire::ProducerModeWire::Chained,
            ProducerMode::Monotonic => wire::ProducerModeWire::Monotonic,
            ProducerMode::Stateless => {
                return Err(EventBrokerError::InvalidProducerOptions {
                    detail: "a stateless producer is not registered".to_owned(),
                    instance: String::new(),
                });
            }
        };
        let body = wire::RegisterProducerWire {
            mode,
            client_agent: client_agent.to_owned(),
        };
        let rb = self
            .authed(self.http().post(&self.url(&format!("{V1}/producers"))), ctx)
            .json(&body)
            .map_err(to_transport)?;
        let resp: wire::RegisterProducerResponseWire = self.send_json(rb).await?;
        Ok(ProducerId(resp.id))
    }

    async fn publish(
        &self,
        ctx: &SecurityContext,
        event: &Event,
    ) -> Result<IngestOutcome, EventBrokerError> {
        let body = wire::PublishEventWire::from_event(event);
        let rb = self
            .authed(self.http().post(&self.url(&format!("{V1}/events"))), ctx)
            .json(&body)
            .map_err(to_transport)?;
        // 201 = persisted (sync path), 202 = accepted. Duplicate is not carried
        // on the wire (design - Branch gaps).
        let status = self.send_status(rb).await?;
        Ok(if status == 201 {
            IngestOutcome::Persisted
        } else {
            IngestOutcome::Accepted
        })
    }

    async fn publish_sync(
        &self,
        ctx: &SecurityContext,
        event: &Event,
    ) -> Result<IngestOutcome, EventBrokerError> {
        let body = wire::PublishEventWire::from_event(event);
        let rb = self
            .authed(self.http().post(&self.url(&format!("{V1}/events"))), ctx)
            .header("Sync-Wait", "true")
            .json(&body)
            .map_err(to_transport)?;
        let status = self.send_status(rb).await?;
        Ok(if status == 201 {
            IngestOutcome::Persisted
        } else {
            IngestOutcome::Accepted
        })
    }

    async fn publish_batch(
        &self,
        ctx: &SecurityContext,
        events: &[Event],
    ) -> Result<IngestOutcome, EventBrokerError> {
        let body = wire::PublishBatchWire {
            events: events.iter().map(wire::PublishEventWire::from_event).collect(),
        };
        let rb = self
            .authed(self.http().post(&self.url(&format!("{V1}/events:batch"))), ctx)
            .json(&body)
            .map_err(to_transport)?;
        // A batch is all-or-nothing; the wire reports one status for the whole
        // batch (201 persisted / 202 accepted), no per-event body.
        let status = self.send_status(rb).await?;
        Ok(if status == 201 {
            IngestOutcome::Persisted
        } else {
            IngestOutcome::Accepted
        })
    }

    async fn get_producer_cursors(
        &self,
        ctx: &SecurityContext,
        producer_id: ProducerId,
    ) -> Result<ProducerCursors, EventBrokerError> {
        let rb = self.authed(
            self.http()
                .get(&self.url(&format!("{V1}/producers/{}/cursors", producer_id.0))),
            ctx,
        );
        let resp: wire::ProducerCursorsResponseWire = self.send_json(rb).await?;
        Ok(resp.into())
    }

    async fn reset_producer_chain(
        &self,
        ctx: &SecurityContext,
        producer_id: ProducerId,
        scope: ResetScope<'_>,
    ) -> Result<(), EventBrokerError> {
        let body = wire::ResetProducerWire::from_scope(scope);
        let rb = self
            .authed(
                self.http()
                    .post(&self.url(&format!("{V1}/producers/{}:reset", producer_id.0))),
                ctx,
            )
            .json(&body)
            .map_err(to_transport)?;
        self.send_status(rb).await?;
        Ok(())
    }

    async fn create_consumer_group(
        &self,
        ctx: &SecurityContext,
        req: CreateConsumerGroupRequest,
    ) -> Result<ConsumerGroup, EventBrokerError> {
        let body = wire::CreateConsumerGroupWire::from_request(req);
        let rb = self
            .authed(
                self.http().post(&self.url(&format!("{V1}/consumer-groups"))),
                ctx,
            )
            .json(&body)
            .map_err(to_transport)?;
        let resp: wire::ConsumerGroupWire = self.send_json(rb).await?;
        consumer_group_from_wire(resp)
    }

    async fn get_consumer_group(
        &self,
        ctx: &SecurityContext,
        id: &ConsumerGroupId,
    ) -> Result<ConsumerGroup, EventBrokerError> {
        let rb = self.authed(
            self.http()
                .get(&self.url(&format!("{V1}/consumer-groups/{}", id.to_gts()))),
            ctx,
        );
        let resp: wire::ConsumerGroupWire = self.send_json(rb).await?;
        consumer_group_from_wire(resp)
    }

    async fn list_consumer_groups(
        &self,
        ctx: &SecurityContext,
        query: ConsumerGroupQuery,
    ) -> Result<Page<ConsumerGroup>, EventBrokerError> {
        let mut params: Vec<(String, String)> = Vec::new();
        if let Some(limit) = query.limit {
            params.push(("limit".to_owned(), limit.to_string()));
        }
        if let Some(cursor) = query.cursor {
            params.push(("cursor".to_owned(), cursor));
        }
        if let Some(filter) = query.filter {
            params.push(("$filter".to_owned(), filter));
        }
        if let Some(orderby) = query.orderby {
            params.push(("$orderby".to_owned(), orderby));
        }
        let qs = serde_html_form::to_string(&params).unwrap_or_default();
        let url = if qs.is_empty() {
            self.url(&format!("{V1}/consumer-groups"))
        } else {
            self.url(&format!("{V1}/consumer-groups?{qs}"))
        };
        let rb = self.authed(self.http().get(&url), ctx);
        let page: wire::PageWire<wire::ConsumerGroupWire> = self.send_json(rb).await?;
        Ok(Page {
            items: page
                .items
                .into_iter()
                .map(consumer_group_from_wire)
                .collect::<Result<Vec<_>, _>>()?,
            next_cursor: page.page_info.next_cursor,
            prev_cursor: page.page_info.prev_cursor,
            limit: u32::try_from(page.page_info.limit).unwrap_or(u32::MAX),
        })
    }

    async fn delete_consumer_group(
        &self,
        ctx: &SecurityContext,
        id: &ConsumerGroupId,
    ) -> Result<(), EventBrokerError> {
        let rb = self.authed(
            self.http()
                .delete(&self.url(&format!("{V1}/consumer-groups/{}", id.to_gts()))),
            ctx,
        );
        self.send_status(rb).await?;
        Ok(())
    }

    async fn join(
        &self,
        ctx: &SecurityContext,
        req: JoinRequest,
    ) -> Result<SubscriptionAssignment, EventBrokerError> {
        let body = wire::JoinSubscriptionWire {
            consumer_group: req.group.to_gts(),
            client_agent: req.client_agent,
            interests: req.interests.iter().map(interest_to_wire).collect(),
            session_timeout: req
                .session_timeout
                .map(|d| format!("PT{}S", d.as_secs())),
        };
        let rb = self
            .authed(
                self.http().post(&self.url(&format!("{V1}/subscriptions"))),
                ctx,
            )
            .json(&body)
            .map_err(to_transport)?;
        let resp: wire::SubscriptionWire = self.send_json(rb).await?;
        Ok(SubscriptionAssignment {
            subscription_id: SubscriptionId(resp.id),
            topology_version: resp.topology_version,
            expires_at: resp.expires_at,
            assigned: resp
                .assigned
                .into_iter()
                .map(|a| crate::api::AssignedPartition {
                    topic: a.topic,
                    partition: u32::try_from(a.partition).unwrap_or_default(),
                })
                .collect(),
        })
    }

    async fn get_subscription(
        &self,
        ctx: &SecurityContext,
        id: SubscriptionId,
    ) -> Result<Subscription, EventBrokerError> {
        let rb = self.authed(
            self.http()
                .get(&self.url(&format!("{V1}/subscriptions/{id}"))),
            ctx,
        );
        let resp: wire::SubscriptionWire = self.send_json(rb).await?;
        subscription_from_wire(resp)
    }

    async fn list_subscriptions(
        &self,
        ctx: &SecurityContext,
    ) -> Result<Vec<Subscription>, EventBrokerError> {
        let wires: Vec<wire::SubscriptionWire> =
            self.list_all(ctx, &format!("{V1}/subscriptions")).await?;
        wires.into_iter().map(subscription_from_wire).collect()
    }

    async fn leave(
        &self,
        ctx: &SecurityContext,
        id: SubscriptionId,
    ) -> Result<(), EventBrokerError> {
        let rb = self.authed(
            self.http()
                .delete(&self.url(&format!("{V1}/subscriptions/{id}"))),
            ctx,
        );
        self.send_status(rb).await?;
        Ok(())
    }

    async fn stream(
        &self,
        ctx: &SecurityContext,
        id: SubscriptionId,
    ) -> Result<crate::api::FrameStream, EventBrokerError> {
        stream::open(self, ctx, id).await
    }

    async fn seek(
        &self,
        ctx: &SecurityContext,
        id: SubscriptionId,
        positions: &[SeekPosition],
    ) -> Result<Vec<SeekResult>, EventBrokerError> {
        let body = wire::SeekSubscriptionWire {
            partition_positions: positions.iter().map(wire::PartitionPositionWire::from).collect(),
        };
        let rb = self
            .authed(
                self.http()
                    .post(&self.url(&format!("{V1}/subscriptions/{id}:seek"))),
                ctx,
            )
            .json(&body)
            .map_err(to_transport)?;
        let resolved: Vec<wire::ResolvedPositionWire> = self.send_json(rb).await?;
        Ok(resolved.into_iter().map(SeekResult::from).collect())
    }

    async fn list_topics(&self, ctx: &SecurityContext) -> Result<Vec<Topic>, EventBrokerError> {
        let wires: Vec<wire::TopicWire> = self.list_all(ctx, &format!("{V1}/topics")).await?;
        wires.into_iter().map(topic_from_wire).collect()
    }

    async fn list_topic_segments(
        &self,
        ctx: &SecurityContext,
        topic: &str,
        partition: u32,
        range: PartitionRange,
    ) -> Result<TopicSegment, EventBrokerError> {
        let mut params: Vec<(String, String)> = vec![
            ("topic".to_owned(), topic.to_owned()),
            ("partition".to_owned(), partition.to_string()),
        ];
        if let Some(start) = range.start_offset {
            params.push(("start_offset".to_owned(), start.as_i64().to_string()));
        }
        if let Some(end) = range.end_offset {
            params.push(("end_offset".to_owned(), end.as_i64().to_string()));
        }
        params.push(("limit".to_owned(), range.limit.to_string()));
        let qs = serde_html_form::to_string(&params).unwrap_or_default();
        let rb = self.authed(
            self.http()
                .get(&self.url(&format!("{V1}/topics/segments?{qs}"))),
            ctx,
        );
        // The wire response is a single manifest object (one per
        // `(topic, partition)`), not a page - see `docs/openapi.yaml`.
        let m: wire::TopicSegmentsResponseWire = self.send_json(rb).await?;
        Ok(TopicSegment {
            topic: m.topic,
            partition: u32::try_from(m.partition).unwrap_or_default(),
            start_sequence: Sequence::assigned(m.start_sequence),
            end_sequence: Sequence::assigned(m.end_sequence),
            start_time: m.start_time,
            end_time: m.end_time,
            segments: m.segments,
        })
    }

    async fn list_event_types(
        &self,
        ctx: &SecurityContext,
    ) -> Result<Vec<EventType>, EventBrokerError> {
        let wires: Vec<wire::EventTypeWire> =
            self.list_all(ctx, &format!("{V1}/event-types")).await?;
        wires.into_iter().map(event_type_from_wire).collect()
    }

    async fn get_event_type(
        &self,
        ctx: &SecurityContext,
        id: &str,
    ) -> Result<EventType, EventBrokerError> {
        // No dedicated route: fetch the list and select by id (design - Branch
        // gaps).
        let wires: Vec<wire::EventTypeWire> =
            self.list_all(ctx, &format!("{V1}/event-types")).await?;
        let found = wires.into_iter().find(|w| w.id == id).ok_or_else(|| {
            EventBrokerError::EventTypeUnknown {
                type_id: id.to_owned(),
                detail: "no such event type".to_owned(),
                instance: String::new(),
            }
        })?;
        event_type_from_wire(found)
    }
}
