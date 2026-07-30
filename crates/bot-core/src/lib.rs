//! Platform-independent runtime primitives for `BroKnowMyQQBot`.

#![forbid(unsafe_code)]

pub mod adapter;
pub mod context;
pub mod model;
pub mod runtime;
pub mod shutdown;

pub use adapter::{Adapter, AdapterError};
pub use context::{Context, ContextError};
pub use model::{
    Action, ActionResult, AdapterId, CommonMessage, Event, EventEnvelope, EventId, MessageScope,
    MessageSegment, MessageTarget, ReplyAction, SendMessageAction, Sender,
};
pub use runtime::{EventHandler, HandlerError, Runtime, RuntimeBuilder, RuntimeError};
pub use shutdown::{ShutdownHandle, ShutdownSignal, shutdown_channel};
