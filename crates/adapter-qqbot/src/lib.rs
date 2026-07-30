//! Adapter connecting QQ Official Bot transports to `bot-core`.

#![forbid(unsafe_code)]

mod mapping;
mod websocket;

pub use websocket::{QqWebSocketAdapter, QqWebSocketConfig};
