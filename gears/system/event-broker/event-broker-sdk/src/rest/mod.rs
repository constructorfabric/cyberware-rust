//! REST transport implementation of `EventBrokerApi`.
//!
//! Gated behind the `rest-client` feature. Keeps all wire, HTTP, and serde
//! concerns off the public SDK surface: the public trait and models stay
//! transport-free, and everything that speaks the wire lives here.

mod client;
mod error;
mod stream;
mod wire;

pub use client::{RestBroker, StreamTransport};
