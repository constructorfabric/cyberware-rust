//! API v1 namespace with endpoint factory methods.

use http::Method;

use super::harness::EventBrokerHarness;
use super::request::RequestCase;

/// Endpoint factory for the `/event-broker/v1/` API surface.
pub struct ApiV1<'a> {
    harness: &'a EventBrokerHarness,
}

impl<'a> ApiV1<'a> {
    pub(crate) fn new(harness: &'a EventBrokerHarness) -> Self {
        Self { harness }
    }

    // -- Ingest: events --

    pub fn post_events(&self) -> RequestCase<'a> {
        RequestCase::new(self.harness, Method::POST, "/event-broker/v1/events")
    }

    pub fn post_events_batch(&self) -> RequestCase<'a> {
        RequestCase::new(self.harness, Method::POST, "/event-broker/v1/events:batch")
    }

    // -- Ingest: producers --

    pub fn post_producers(&self) -> RequestCase<'a> {
        RequestCase::new(self.harness, Method::POST, "/event-broker/v1/producers")
    }

    pub fn get_producer_cursors(&self, id: &str) -> RequestCase<'a> {
        RequestCase::new(
            self.harness,
            Method::GET,
            format!("/event-broker/v1/producers/{id}/cursors"),
        )
    }

    pub fn post_producer_reset(&self, id: &str) -> RequestCase<'a> {
        RequestCase::new(
            self.harness,
            Method::POST,
            format!("/event-broker/v1/producers/{id}:reset"),
        )
    }

    // -- Shared: topics / event-types --

    pub fn get_topics(&self) -> RequestCase<'a> {
        RequestCase::new(self.harness, Method::GET, "/event-broker/v1/topics")
    }

    pub fn get_topic_segments(&self) -> RequestCase<'a> {
        RequestCase::new(
            self.harness,
            Method::GET,
            "/event-broker/v1/topics/segments",
        )
    }

    pub fn get_event_types(&self) -> RequestCase<'a> {
        RequestCase::new(self.harness, Method::GET, "/event-broker/v1/event-types")
    }

    // -- Delivery: consumer-groups --

    pub fn post_consumer_groups(&self) -> RequestCase<'a> {
        RequestCase::new(
            self.harness,
            Method::POST,
            "/event-broker/v1/consumer-groups",
        )
    }

    pub fn get_consumer_groups(&self) -> RequestCase<'a> {
        RequestCase::new(
            self.harness,
            Method::GET,
            "/event-broker/v1/consumer-groups",
        )
    }

    pub fn get_consumer_group(&self, id: &str) -> RequestCase<'a> {
        RequestCase::new(
            self.harness,
            Method::GET,
            format!("/event-broker/v1/consumer-groups/{id}"),
        )
    }

    pub fn delete_consumer_group(&self, id: &str) -> RequestCase<'a> {
        RequestCase::new(
            self.harness,
            Method::DELETE,
            format!("/event-broker/v1/consumer-groups/{id}"),
        )
    }

    // -- Delivery: subscriptions --

    pub fn post_subscriptions(&self) -> RequestCase<'a> {
        RequestCase::new(self.harness, Method::POST, "/event-broker/v1/subscriptions")
    }

    pub fn get_subscriptions(&self) -> RequestCase<'a> {
        RequestCase::new(self.harness, Method::GET, "/event-broker/v1/subscriptions")
    }

    pub fn get_subscription(&self, id: &str) -> RequestCase<'a> {
        RequestCase::new(
            self.harness,
            Method::GET,
            format!("/event-broker/v1/subscriptions/{id}"),
        )
    }

    pub fn delete_subscription(&self, id: &str) -> RequestCase<'a> {
        RequestCase::new(
            self.harness,
            Method::DELETE,
            format!("/event-broker/v1/subscriptions/{id}"),
        )
    }

    pub fn post_subscription_seek(&self, id: &str) -> RequestCase<'a> {
        RequestCase::new(
            self.harness,
            Method::POST,
            format!("/event-broker/v1/subscriptions/{id}:seek"),
        )
    }

    // -- Delivery: streaming --

    pub fn get_events_stream(&self, subscription_id: &str) -> RequestCase<'a> {
        RequestCase::new(self.harness, Method::GET, "/event-broker/v1/events:stream")
            .with_query("subscription_id", subscription_id)
    }

    pub fn get_events_sse(&self, subscription_id: &str) -> RequestCase<'a> {
        RequestCase::new(self.harness, Method::GET, "/event-broker/v1/events:sse")
            .with_query("subscription_id", subscription_id)
    }
}
