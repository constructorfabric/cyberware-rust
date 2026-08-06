//! Response wrapper with assertion helpers for integration tests.

use axum::body::Body;
use http::StatusCode;
use http::header::HeaderMap;
use serde::de::DeserializeOwned;

/// Eagerly-collected HTTP response with sync assertion methods.
pub struct TestResponse {
    status: StatusCode,
    headers: HeaderMap,
    body_bytes: Vec<u8>,
}

impl TestResponse {
    /// Consume an `http::Response<Body>`, collecting the body into bytes.
    pub async fn from_response(resp: http::Response<Body>) -> Self {
        let (parts, body) = resp.into_parts();
        let body_bytes = axum::body::to_bytes(body, usize::MAX)
            .await
            .expect("failed to collect response body")
            .to_vec();
        Self {
            status: parts.status,
            headers: parts.headers,
            body_bytes,
        }
    }

    // -- Assertions --

    pub fn assert_status(&self, expected: u16) -> &Self {
        assert_eq!(
            self.status.as_u16(),
            expected,
            "expected status {expected}, got {}. Body: {}",
            self.status.as_u16(),
            String::from_utf8_lossy(&self.body_bytes),
        );
        self
    }

    pub fn assert_header(&self, name: &str, expected: &str) -> &Self {
        let actual = self
            .headers
            .get(name)
            .unwrap_or_else(|| panic!("header '{name}' not present in response"))
            .to_str()
            .unwrap_or_else(|_| panic!("header '{name}' is not valid UTF-8"));
        assert_eq!(
            actual, expected,
            "header '{name}': expected '{expected}', got '{actual}'"
        );
        self
    }

    pub fn assert_body_contains(&self, needle: &str) -> &Self {
        let body_str = String::from_utf8_lossy(&self.body_bytes);
        assert!(
            body_str.contains(needle),
            "expected body to contain '{needle}', body was: {body_str}"
        );
        self
    }

    // -- Body parsing --

    pub fn status(&self) -> StatusCode {
        self.status
    }

    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    pub fn bytes(&self) -> &[u8] {
        &self.body_bytes
    }

    pub fn text(&self) -> String {
        String::from_utf8(self.body_bytes.clone()).expect("response body is not valid UTF-8")
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body_bytes).expect("response body is not valid JSON")
    }

    pub fn parse<T: DeserializeOwned>(&self) -> T {
        serde_json::from_slice(&self.body_bytes).unwrap_or_else(|e| {
            panic!(
                "failed to deserialize response body as {}: {e}\nbody: {}",
                std::any::type_name::<T>(),
                String::from_utf8_lossy(&self.body_bytes),
            )
        })
    }
}
