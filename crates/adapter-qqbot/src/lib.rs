//! Adapter connecting QQ Official Bot transports to `bot-core`.

#![forbid(unsafe_code)]

mod mapping;
mod webhook;
mod websocket;

pub use webhook::{QqWebhookAdapter, QqWebhookConfig, is_literal_http_path};
pub use websocket::{QqWebSocketAdapter, QqWebSocketConfig};
