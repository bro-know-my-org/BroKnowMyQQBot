//! Adapter supervision and event dispatch.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use async_trait::async_trait;
use thiserror::Error;
use tokio::{
    sync::{Semaphore, mpsc, oneshot},
    task::JoinSet,
    time::timeout,
};
use tracing::{error, info, warn};

use crate::{Adapter, AdapterError, AdapterId, Context, Event, ShutdownSignal};

#[derive(Debug, Error)]
pub enum HandlerError {
    #[error("handler failed: {0}")]
    Failed(String),
}

#[async_trait]
pub trait EventHandler: Send + Sync + 'static {
    fn name(&self) -> &str;

    async fn handle(&self, context: Context, event: &Event) -> Result<(), HandlerError>;
}

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("duplicate adapter ID `{0}`")]
    DuplicateAdapter(AdapterId),
    #[error("runtime requires at least one adapter")]
    NoAdapters,
    #[error("all adapters stopped before shutdown")]
    AllAdaptersStopped,
    #[error("adapter task failed to join: {0}")]
    AdapterJoin(String),
    #[error("adapter `{adapter_id}` stopped with an error: {message}")]
    AdapterStopped {
        adapter_id: AdapterId,
        message: String,
    },
    #[error("event dispatch task failed to join: {0}")]
    EventJoin(String),
}

pub struct RuntimeBuilder {
    queue_capacity: usize,
    handler_timeout: Duration,
    shutdown_timeout: Duration,
    event_concurrency: usize,
    adapters: Vec<Arc<dyn Adapter>>,
    handlers: Vec<Arc<dyn EventHandler>>,
}

impl std::fmt::Debug for RuntimeBuilder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RuntimeBuilder")
            .field("queue_capacity", &self.queue_capacity)
            .field("handler_timeout", &self.handler_timeout)
            .field("shutdown_timeout", &self.shutdown_timeout)
            .field("event_concurrency", &self.event_concurrency)
            .field("adapter_count", &self.adapters.len())
            .field("handler_count", &self.handlers.len())
            .finish()
    }
}

impl Default for RuntimeBuilder {
    fn default() -> Self {
        Self {
            queue_capacity: 1024,
            handler_timeout: Duration::from_secs(30),
            shutdown_timeout: Duration::from_secs(20),
            event_concurrency: 32,
            adapters: Vec::new(),
            handlers: Vec::new(),
        }
    }
}

impl RuntimeBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn queue_capacity(mut self, capacity: usize) -> Self {
        self.queue_capacity = capacity.max(1);
        self
    }

    #[must_use]
    pub fn handler_timeout(mut self, duration: Duration) -> Self {
        self.handler_timeout = duration;
        self
    }

    #[must_use]
    pub fn shutdown_timeout(mut self, duration: Duration) -> Self {
        self.shutdown_timeout = duration;
        self
    }

    #[must_use]
    pub fn event_concurrency(mut self, concurrency: usize) -> Self {
        self.event_concurrency = concurrency.max(1);
        self
    }

    #[must_use]
    pub fn adapter(mut self, adapter: Arc<dyn Adapter>) -> Self {
        self.adapters.push(adapter);
        self
    }

    #[must_use]
    pub fn handler(mut self, handler: Arc<dyn EventHandler>) -> Self {
        self.handlers.push(handler);
        self
    }

    pub fn build(self) -> Result<Runtime, RuntimeError> {
        if self.adapters.is_empty() {
            return Err(RuntimeError::NoAdapters);
        }
        let mut ids = HashSet::new();
        for adapter in &self.adapters {
            if !ids.insert(adapter.id().clone()) {
                return Err(RuntimeError::DuplicateAdapter(adapter.id().clone()));
            }
        }
        Ok(Runtime {
            queue_capacity: self.queue_capacity,
            handler_timeout: self.handler_timeout,
            shutdown_timeout: self.shutdown_timeout,
            event_concurrency: self.event_concurrency,
            adapters: self.adapters,
            handlers: self.handlers,
        })
    }
}

pub struct Runtime {
    queue_capacity: usize,
    handler_timeout: Duration,
    shutdown_timeout: Duration,
    event_concurrency: usize,
    adapters: Vec<Arc<dyn Adapter>>,
    handlers: Vec<Arc<dyn EventHandler>>,
}

type PartitionTails = Arc<StdMutex<HashMap<String, (u64, oneshot::Receiver<()>)>>>;

struct EventDispatcher {
    queue_capacity: usize,
    handler_timeout: Duration,
    adapters: Arc<Vec<Arc<dyn Adapter>>>,
    handlers: Arc<Vec<Arc<dyn EventHandler>>>,
    active_events: Arc<Semaphore>,
    partition_tails: PartitionTails,
    dispatch_generation: u64,
}

impl EventDispatcher {
    fn new(
        queue_capacity: usize,
        handler_timeout: Duration,
        event_concurrency: usize,
        adapters: Arc<Vec<Arc<dyn Adapter>>>,
        handlers: Arc<Vec<Arc<dyn EventHandler>>>,
    ) -> Self {
        Self {
            queue_capacity,
            handler_timeout,
            adapters,
            handlers,
            active_events: Arc::new(Semaphore::new(event_concurrency)),
            partition_tails: Arc::new(StdMutex::new(HashMap::new())),
            dispatch_generation: 0,
        }
    }

    async fn run(
        mut self,
        mut event_receiver: mpsc::Receiver<crate::EventEnvelope>,
        shutdown: &mut ShutdownSignal,
        adapter_tasks: &mut JoinSet<(AdapterId, Result<(), AdapterError>)>,
    ) -> (JoinSet<()>, Option<RuntimeError>) {
        let mut event_tasks = JoinSet::new();
        loop {
            if event_tasks.len() >= self.queue_capacity {
                tokio::select! {
                    () = shutdown.cancelled() => {
                        event_receiver.close();
                        while let Some(event) = event_receiver.recv().await {
                            self.spawn_event(&mut event_tasks, event);
                        }
                        break;
                    },
                    joined = event_tasks.join_next() => {
                        if let Err(error) = check_event_join(joined) {
                            return (event_tasks, Some(error));
                        }
                    }
                    joined = adapter_tasks.join_next(), if !adapter_tasks.is_empty() => {
                        if let Err(error) = check_adapter_join(joined) {
                            return (event_tasks, Some(error));
                        }
                    }
                }
                continue;
            }
            tokio::select! {
                () = shutdown.cancelled() => {
                    event_receiver.close();
                    while let Some(event) = event_receiver.recv().await {
                        self.spawn_event(&mut event_tasks, event);
                    }
                    break;
                },
                joined = event_tasks.join_next(), if !event_tasks.is_empty() => {
                    if let Err(error) = check_event_join(joined) {
                        return (event_tasks, Some(error));
                    }
                }
                joined = adapter_tasks.join_next(), if !adapter_tasks.is_empty() => {
                    if let Err(error) = check_adapter_join(joined) {
                        return (event_tasks, Some(error));
                    }
                }
                event = event_receiver.recv() => {
                    let Some(event) = event else {
                        if shutdown.is_shutdown() {
                            break;
                        }
                        while let Some(joined) = adapter_tasks.try_join_next() {
                            if let Err(error) = check_adapter_join(Some(joined)) {
                                return (event_tasks, Some(error));
                            }
                        }
                        return (event_tasks, Some(RuntimeError::AllAdaptersStopped));
                    };
                    self.spawn_event(&mut event_tasks, event);
                }
            }
        }
        (event_tasks, None)
    }

    fn spawn_event(&mut self, event_tasks: &mut JoinSet<()>, event: crate::EventEnvelope) {
        let Some(adapter) = self
            .adapters
            .iter()
            .find(|adapter| adapter.id() == &event.adapter)
            .cloned()
        else {
            warn!(adapter_id = %event.adapter, event_id = %event.id, "dropping event from unknown adapter");
            return;
        };
        let handlers = Arc::clone(&self.handlers);
        let active_events = Arc::clone(&self.active_events);
        let partition_tails = Arc::clone(&self.partition_tails);
        let handler_timeout = self.handler_timeout;
        let partition_key = runtime_partition_key(&event);
        self.dispatch_generation = self.dispatch_generation.wrapping_add(1);
        let generation = self.dispatch_generation;
        let (finished_sender, finished_receiver) = oneshot::channel();
        let previous = {
            let mut tails = self
                .partition_tails
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let previous = tails.remove(&partition_key).map(|(_, receiver)| receiver);
            tails.insert(partition_key.clone(), (generation, finished_receiver));
            previous
        };
        event_tasks.spawn(async move {
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            let Ok(_permit) = active_events.acquire_owned().await else {
                return;
            };
            process_event(event, adapter, handlers, handler_timeout).await;
            let _ = finished_sender.send(());
            if let Ok(mut tails) = partition_tails.lock()
                && tails
                    .get(&partition_key)
                    .is_some_and(|(current, _)| *current == generation)
            {
                tails.remove(&partition_key);
            }
        });
    }
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Runtime")
            .field("queue_capacity", &self.queue_capacity)
            .field("handler_timeout", &self.handler_timeout)
            .field("shutdown_timeout", &self.shutdown_timeout)
            .field("event_concurrency", &self.event_concurrency)
            .field("adapter_count", &self.adapters.len())
            .field("handler_count", &self.handlers.len())
            .finish()
    }
}

impl Runtime {
    pub async fn run(self, mut shutdown: ShutdownSignal) -> Result<(), RuntimeError> {
        let (event_sender, event_receiver) = mpsc::channel(self.queue_capacity);
        let mut adapter_tasks = JoinSet::new();

        for adapter in &self.adapters {
            let adapter = Arc::clone(adapter);
            let events = event_sender.clone();
            let adapter_shutdown = shutdown.clone();
            adapter_tasks.spawn(async move {
                let id = adapter.id().clone();
                let result = adapter.run(events, adapter_shutdown).await;
                (id, result)
            });
        }
        drop(event_sender);

        let adapters = Arc::new(self.adapters);
        let handlers = Arc::new(self.handlers);
        info!(
            adapter_count = adapters.len(),
            event_concurrency = self.event_concurrency,
            "bot runtime started"
        );
        let dispatcher = EventDispatcher::new(
            self.queue_capacity,
            self.handler_timeout,
            self.event_concurrency,
            Arc::clone(&adapters),
            handlers,
        );
        let (mut event_tasks, mut runtime_error) = dispatcher
            .run(event_receiver, &mut shutdown, &mut adapter_tasks)
            .await;

        info!("bot runtime is shutting down");
        let finish_events = async {
            while let Some(joined) = event_tasks.join_next().await {
                joined.map_err(|error| RuntimeError::EventJoin(error.to_string()))?;
            }
            Ok::<(), RuntimeError>(())
        };
        match timeout(self.shutdown_timeout, finish_events).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                runtime_error.get_or_insert(error);
            }
            Err(_) => {
                warn!("event dispatch shutdown timed out; aborting remaining tasks");
                event_tasks.abort_all();
                while event_tasks.join_next().await.is_some() {}
            }
        }
        if runtime_error.is_some() {
            adapter_tasks.abort_all();
        }
        let finish_adapters = async {
            while let Some(joined) = adapter_tasks.join_next().await {
                let (adapter_id, result) =
                    joined.map_err(|error| RuntimeError::AdapterJoin(error.to_string()))?;
                if let Err(error) = result {
                    log_adapter_error(&adapter_id, &error);
                    return Err(RuntimeError::AdapterStopped {
                        adapter_id,
                        message: error.to_string(),
                    });
                }
            }
            Ok::<(), RuntimeError>(())
        };
        match timeout(self.shutdown_timeout, finish_adapters).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                runtime_error.get_or_insert(error);
            }
            Err(_) => {
                warn!("adapter shutdown timed out; aborting remaining tasks");
                adapter_tasks.abort_all();
                while adapter_tasks.join_next().await.is_some() {}
            }
        }
        runtime_error.map_or(Ok(()), Err)
    }
}

fn check_event_join(
    joined: Option<Result<(), tokio::task::JoinError>>,
) -> Result<(), RuntimeError> {
    if let Some(joined) = joined {
        joined.map_err(|error| RuntimeError::EventJoin(error.to_string()))?;
    }
    Ok(())
}

type AdapterTaskResult = Result<(AdapterId, Result<(), AdapterError>), tokio::task::JoinError>;

fn check_adapter_join(joined: Option<AdapterTaskResult>) -> Result<(), RuntimeError> {
    let Some(joined) = joined else {
        return Ok(());
    };
    let (adapter_id, result) =
        joined.map_err(|error| RuntimeError::AdapterJoin(error.to_string()))?;
    match result {
        Ok(()) => {
            info!(%adapter_id, "adapter stopped");
            Ok(())
        }
        Err(error) => {
            log_adapter_error(&adapter_id, &error);
            Err(RuntimeError::AdapterStopped {
                adapter_id,
                message: error.to_string(),
            })
        }
    }
}

fn runtime_partition_key(event: &crate::EventEnvelope) -> String {
    let adapter = event.adapter.as_str();
    match &event.event {
        Event::Message(message) => match &message.target {
            crate::MessageTarget::Group { group_id } => format!("{adapter}:group:{group_id}"),
            crate::MessageTarget::Private { user_id } => format!("{adapter}:private:{user_id}"),
            crate::MessageTarget::Channel { channel_id } => {
                format!("{adapter}:channel:{channel_id}")
            }
        },
        _ => format!("{adapter}:event:{}", event.id),
    }
}

async fn process_event(
    event: crate::EventEnvelope,
    adapter: Arc<dyn Adapter>,
    handlers: Arc<Vec<Arc<dyn EventHandler>>>,
    handler_timeout: Duration,
) {
    let context = Context::new(&event, Arc::clone(&adapter));
    let mut handled_successfully = true;
    for handler in handlers.iter() {
        let result = timeout(
            handler_timeout,
            handler.handle(context.clone(), &event.event),
        )
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                handled_successfully = false;
                error!(
                    handler = handler.name(),
                    event_id = %event.id,
                    error = %error,
                    "event handler failed"
                );
            }
            Err(_) => {
                handled_successfully = false;
                error!(
                    handler = handler.name(),
                    event_id = %event.id,
                    "event handler timed out"
                );
            }
        }
    }
    if handled_successfully && let Err(error) = adapter.event_handled(&event).await {
        error!(
            adapter_id = %event.adapter,
            event_id = %event.id,
            error = %error,
            "adapter failed to commit handled event"
        );
    }
}

fn log_adapter_error(adapter_id: &AdapterId, error: &AdapterError) {
    error!(adapter_id = %adapter_id, error = %error, "adapter stopped with an error");
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use serde_json::{Value, json};
    use tokio::sync::{Mutex, mpsc};

    use crate::{
        Action, ActionResult, Adapter, AdapterError, AdapterId, CommonMessage, Context, Event,
        EventEnvelope, EventHandler, EventId, HandlerError, MessageSegment, MessageTarget,
        RuntimeBuilder, Sender, ShutdownHandle, ShutdownSignal, shutdown_channel,
    };

    #[derive(Debug)]
    struct MockAdapter {
        id: AdapterId,
        actions: mpsc::Sender<Action>,
        event: Mutex<Option<EventEnvelope>>,
    }

    #[async_trait]
    impl Adapter for MockAdapter {
        fn id(&self) -> &AdapterId {
            &self.id
        }

        fn platform(&self) -> &'static str {
            "mock"
        }

        async fn run(
            &self,
            events: mpsc::Sender<EventEnvelope>,
            mut shutdown: ShutdownSignal,
        ) -> Result<(), AdapterError> {
            if let Some(event) = self.event.lock().await.take() {
                events
                    .send(event)
                    .await
                    .map_err(|_| AdapterError::EventQueueClosed)?;
            }
            shutdown.cancelled().await;
            Ok(())
        }

        async fn execute(&self, action: Action) -> Result<ActionResult, AdapterError> {
            self.actions
                .send(action)
                .await
                .map_err(|_| AdapterError::Action("action observer closed".to_owned()))?;
            Ok(ActionResult {
                message_id: Some("sent-message".to_owned()),
                raw: Value::Null,
            })
        }
    }

    #[derive(Debug)]
    struct PingHandler {
        name: String,
        shutdown: ShutdownHandle,
    }

    #[async_trait]
    impl EventHandler for PingHandler {
        fn name(&self) -> &str {
            &self.name
        }

        async fn handle(&self, context: Context, event: &Event) -> Result<(), HandlerError> {
            if let Event::Message(message) = event
                && message.text.trim() == "/ping"
            {
                context
                    .reply("pong")
                    .await
                    .map_err(|error| HandlerError::Failed(error.to_string()))?;
                self.shutdown.shutdown();
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn routes_reply_to_source_adapter() {
        let (action_sender, mut actions) = mpsc::channel(1);
        let (shutdown_handle, shutdown_signal) = shutdown_channel();
        let adapter = Arc::new(MockAdapter {
            id: AdapterId::new("mock"),
            actions: action_sender,
            event: Mutex::new(Some(EventEnvelope {
                id: EventId::new("event-1"),
                adapter: AdapterId::new("mock"),
                delivery_id: None,
                timestamp: None,
                event: Event::Message(CommonMessage {
                    message_id: "source-message".to_owned(),
                    target: MessageTarget::Group {
                        group_id: "group".to_owned(),
                    },
                    sender: Sender {
                        id: "user".to_owned(),
                        display_name: None,
                    },
                    text: "/ping".to_owned(),
                    segments: vec![MessageSegment::Text {
                        text: "/ping".to_owned(),
                    }],
                    reply_to: None,
                }),
                raw: json!({"platform":"mock"}),
            })),
        });
        let runtime = RuntimeBuilder::new()
            .adapter(adapter)
            .handler(Arc::new(PingHandler {
                name: "ping".to_owned(),
                shutdown: shutdown_handle,
            }))
            .build()
            .unwrap();

        runtime.run(shutdown_signal).await.unwrap();
        let action = actions.recv().await.unwrap();
        let Action::Reply(reply) = action else {
            panic!("expected reply action");
        };
        assert_eq!(reply.source_message_id, "source-message");
        assert_eq!(reply.content, "pong");
    }
}
