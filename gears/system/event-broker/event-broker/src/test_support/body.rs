//! Body conversion trait for the fluent request builder.

use axum::body::Body;
use http::header::HeaderValue;
use serde::Serialize;

/// Converts a value into a request body, optionally providing a Content-Type header.
pub trait IntoBody {
    fn into_body(self) -> (Body, Option<HeaderValue>);
}

/// Newtype wrapper that signals "serialize this as JSON".
pub struct Json<T>(pub T);

static APPLICATION_JSON: HeaderValue = HeaderValue::from_static("application/json");

impl<T: Serialize> IntoBody for Json<T> {
    fn into_body(self) -> (Body, Option<HeaderValue>) {
        let bytes = serde_json::to_vec(&self.0).expect("failed to serialize body as JSON");
        (Body::from(bytes), Some(APPLICATION_JSON.clone()))
    }
}

impl IntoBody for serde_json::Value {
    fn into_body(self) -> (Body, Option<HeaderValue>) {
        let bytes = serde_json::to_vec(&self).expect("failed to serialize JSON Value");
        (Body::from(bytes), Some(APPLICATION_JSON.clone()))
    }
}

impl IntoBody for &str {
    fn into_body(self) -> (Body, Option<HeaderValue>) {
        (Body::from(self.to_owned()), None)
    }
}

impl IntoBody for String {
    fn into_body(self) -> (Body, Option<HeaderValue>) {
        (Body::from(self), None)
    }
}

impl IntoBody for bytes::Bytes {
    fn into_body(self) -> (Body, Option<HeaderValue>) {
        (Body::from(self), None)
    }
}

impl IntoBody for Vec<u8> {
    fn into_body(self) -> (Body, Option<HeaderValue>) {
        (Body::from(self), None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn collect_body(body: Body) -> Vec<u8> {
        axum::body::to_bytes(body, usize::MAX)
            .await
            .unwrap()
            .to_vec()
    }

    #[tokio::test]
    async fn json_serialize_sets_content_type() {
        #[derive(Serialize)]
        struct Payload {
            name: String,
        }

        let (body, ct) = Json(Payload {
            name: "test".into(),
        })
        .into_body();

        assert_eq!(ct.unwrap().as_bytes(), b"application/json");
        let bytes = collect_body(body).await;
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["name"], "test");
    }

    #[tokio::test]
    async fn json_value_sets_content_type() {
        let (body, ct) = serde_json::json!({"key": "val"}).into_body();

        assert_eq!(ct.unwrap().as_bytes(), b"application/json");
        let bytes = collect_body(body).await;
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["key"], "val");
    }

    #[tokio::test]
    async fn str_has_no_content_type() {
        let (body, ct) = "hello".into_body();

        assert!(ct.is_none());
        let bytes = collect_body(body).await;
        assert_eq!(bytes, b"hello");
    }

    #[tokio::test]
    async fn string_has_no_content_type() {
        let (body, ct) = String::from("hello").into_body();

        assert!(ct.is_none());
        let bytes = collect_body(body).await;
        assert_eq!(bytes, b"hello");
    }

    #[tokio::test]
    async fn bytes_has_no_content_type() {
        let (body, ct) = bytes::Bytes::from_static(b"raw").into_body();

        assert!(ct.is_none());
        let bytes = collect_body(body).await;
        assert_eq!(bytes, b"raw");
    }

    #[tokio::test]
    async fn vec_u8_has_no_content_type() {
        let (body, ct) = vec![1u8, 2, 3].into_body();

        assert!(ct.is_none());
        let bytes = collect_body(body).await;
        assert_eq!(bytes, &[1, 2, 3]);
    }
}
