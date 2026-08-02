//! Platform-independent runtime primitives for `BroKnowMyQQBot`.

#![forbid(unsafe_code)]

pub mod adapter;
pub mod context;
pub mod dedup;
pub mod metrics;
pub mod model;
pub mod router;
pub mod runtime;
pub mod shutdown;

pub use adapter::{
    Adapter, AdapterError, EventPermit, EventSendError, EventSender, EventSenderBuildError,
};
pub use context::{Context, ContextError};
pub use dedup::{DedupClaim, DedupError, DedupErrorKind, DedupKey, DedupStore, MemoryDedupStore};
pub use metrics::{
    AdapterFailureKind, HealthStatus, RuntimeHealth, RuntimeMetricsSnapshot, RuntimeObserver,
    RuntimePhase,
};
pub use model::{
    Action, ActionResult, AdapterId, CommonMessage, Event, EventEnvelope, EventId, MessageScope,
    MessageSegment, MessageTarget, ReplyAction, SendMessageAction, Sender,
};
pub use router::{
    CommandHandler, CommandInvocation, CommandRouter, EventFilter, EventKind, EventKindFilter,
    EventMiddleware, EventRoute, EventRouter, MessageScopeFilter, MiddlewareControl, RouteOutcome,
    RouterError,
};
pub use runtime::{
    EventHandler, HandlerError, HandlerPolicy, Runtime, RuntimeBuilder, RuntimeError,
};
pub use shutdown::{ShutdownHandle, ShutdownSignal, shutdown_channel};
