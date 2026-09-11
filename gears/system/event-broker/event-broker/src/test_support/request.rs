//! Fluent request builder for integration tests.

use axum::body::Body;
use http::header::HeaderMap;
use http::{HeaderName, HeaderValue, Method};
use serde::de::DeserializeOwned;
use tower::ServiceExt;

use super::body::IntoBody;
use super::harness::EventBrokerHarness;
use super::response::TestResponse;

/// Fluent HTTP request builder tied to an [`EventBrokerHarness`].
pub struct RequestCase<'a> {
    harness: &'a EventBrokerHarness,
    method: Method,
    path: String,
    headers: HeaderMap,
    query: Vec<(String, String)>,
    body: Option<Body>,
}

impl<'a> RequestCase<'a> {
    pub(crate) fn new(
        harness: &'a EventBrokerHarness,
        method: Method,
        path: impl Into<String>,
    ) -> Self {
        Self {
            harness,
            method,
            path: path.into(),
            headers: HeaderMap::new(),
            query: Vec::new(),
            body: None,
        }
    }

    /// Set the request body. Accepts any type implementing [`IntoBody`]:
    /// `Json(&struct)`, `serde_json::Value`, `&str`, `String`, `Bytes`, `Vec<u8>`.
    pub fn with_body(mut self, body: impl IntoBody) -> Self {
        let (b, ct) = body.into_body();
        self.body = Some(b);
        if let Some(ct) = ct {
            self.headers.insert(http::header::CONTENT_TYPE, ct);
        }
        self
    }

    /// Add a request header.
    pub fn with_header(
        mut self,
        name: impl Into<HeaderName>,
        value: impl Into<HeaderValue>,
    ) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }

    /// Add a query parameter.
    pub fn with_query(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.query.push((key.into(), value.into()));
        self
    }

    /// Send the request and return the collected response.
    pub async fn send(self) -> TestResponse {
        let uri = if self.query.is_empty() {
            self.path
        } else {
            let qs: Vec<String> = self.query.iter().map(|(k, v)| format!("{k}={v}")).collect();
            format!("{}?{}", self.path, qs.join("&"))
        };

        let mut builder = http::Request::builder().method(self.method).uri(&uri);

        for (name, value) in &self.headers {
            builder = builder.header(name, value);
        }

        let body = self.body.unwrap_or_else(Body::empty);
        let request = builder.body(body).expect("failed to build request");

        let router = self.harness.router().clone();
        let response = router
            .oneshot(request)
            .await
            .expect("the service itself must not error");

        TestResponse::from_response(response).await
    }

    /// Send and assert the expected status code, returning the response for
    /// further assertions.
    pub async fn expect_status(self, status: u16) -> TestResponse {
        let resp = self.send().await;
        resp.assert_status(status);
        resp
    }

    /// Send, assert 200 OK, and deserialize the body.
    pub async fn expect_ok<T: DeserializeOwned>(self) -> T {
        let resp = self.send().await;
        resp.assert_status(200);
        resp.parse()
    }
}
