//! Event routes, filters, middleware hooks, and command parsing.

use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet},
    future::Future,
    panic::AssertUnwindSafe,
    sync::Arc,
};

use async_trait::async_trait;
use futures_util::FutureExt as _;
use thiserror::Error;

use crate::{Context, Event, EventHandler, HandlerError, MessageScope};

const MAX_EVENT_ROUTES: usize = 256;
const MAX_ROUTE_FILTERS: usize = 32;
const MAX_ROUTE_MIDDLEWARE: usize = 32;
const MAX_COMMAND_KEYS: usize = 1024;
const MAX_COMMAND_NAME_BYTES: usize = 64;
const MAX_COMMAND_PREFIXES: usize = 32;
const MAX_COMMAND_MENTIONS: usize = 32;
const MAX_COMMAND_TRIGGER_BYTES: usize = 256;
const MAX_COMMAND_TEXT_BYTES: usize = 4096;

pub trait EventFilter: Send + Sync + 'static {
    fn matches(&self, context: &Context, event: &Event) -> bool;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MiddlewareControl {
    Continue,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteOutcome {
    Success,
    Failure,
    Skipped,
}

#[async_trait]
pub trait EventMiddleware: Send + Sync + 'static {
    async fn before(
        &self,
        _context: &Context,
        _event: &Event,
    ) -> Result<MiddlewareControl, HandlerError> {
        Ok(MiddlewareControl::Continue)
    }

    async fn after(
        &self,
        _context: &Context,
        _event: &Event,
        _outcome: RouteOutcome,
    ) -> Result<(), HandlerError> {
        Ok(())
    }

    /// Synchronous cleanup when this middleware's own `before` hook fails.
    fn failed(&self, _context: &Context, _event: &Event) {}

    /// Synchronous cancellation cleanup for resources acquired by `before`.
    ///
    /// This hook is invoked from a Drop guard if route execution or an `after`
    /// future is cancelled before normal middleware unwinding completes.
    fn cancelled(&self, _context: &Context, _event: &Event) {}
}

pub struct EventRoute {
    pub priority: i32,
    pub stop_after_match: bool,
    handler: Arc<dyn EventHandler>,
    filters: Vec<Arc<dyn EventFilter>>,
    middleware: Vec<Arc<dyn EventMiddleware>>,
}

impl std::fmt::Debug for EventRoute {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let handler = handler_name(&self.handler);
        formatter
            .debug_struct("EventRoute")
            .field("priority", &self.priority)
            .field("stop_after_match", &self.stop_after_match)
            .field("handler", &handler)
            .field("filter_count", &self.filters.len())
            .field("middleware_count", &self.middleware.len())
            .finish()
    }
}

impl EventRoute {
    pub fn new(handler: Arc<dyn EventHandler>) -> Self {
        Self {
            priority: 0,
            stop_after_match: false,
            handler,
            filters: Vec::new(),
            middleware: Vec::new(),
        }
    }

    /// Sets execution priority. Lower values run before higher values.
    #[must_use]
    pub fn priority(mut self, priority: i32) -> Self {
        self.priority = priority;
        self
    }

    /// Stops evaluation of later routes after this route matches.
    #[must_use]
    pub fn stop_after_match(mut self, stop: bool) -> Self {
        self.stop_after_match = stop;
        self
    }

    pub fn filter(mut self, filter: Arc<dyn EventFilter>) -> Result<Self, RouterError> {
        if self.filters.len() >= MAX_ROUTE_FILTERS {
            return Err(RouterError::TooManyRouteFilters);
        }
        self.filters.push(filter);
        Ok(self)
    }

    pub fn middleware(mut self, middleware: Arc<dyn EventMiddleware>) -> Result<Self, RouterError> {
        if self.middleware.len() >= MAX_ROUTE_MIDDLEWARE {
            return Err(RouterError::TooManyRouteMiddleware);
        }
        self.middleware.push(middleware);
        Ok(self)
    }
}

/// Routes one at-least-once event through matching handlers in priority order.
///
/// If a later route fails, a redelivery can repeat earlier successful routes.
/// Effectful route handlers must therefore use business idempotency keys; the
/// router does not claim route-level transactions or exactly-once delivery.
pub struct EventRouter {
    name: String,
    routes: Vec<EventRoute>,
}

impl Default for EventRouter {
    fn default() -> Self {
        Self {
            name: "event-router".to_owned(),
            routes: Vec::new(),
        }
    }
}

impl std::fmt::Debug for EventRouter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EventRouter")
            .field("name", &self.name)
            .field("routes", &self.routes)
            .finish()
    }
}

impl EventRouter {
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    pub fn route(mut self, route: EventRoute) -> Result<Self, RouterError> {
        if self.routes.len() >= MAX_EVENT_ROUTES {
            return Err(RouterError::TooManyRoutes);
        }
        self.routes.push(route);
        self.routes.sort_by_key(|route| route.priority);
        Ok(self)
    }
}

#[async_trait]
impl EventHandler for EventRouter {
    fn name(&self) -> &str {
        &self.name
    }

    #[allow(clippy::too_many_lines)] // Keep middleware entry and unwind ordering in one state machine.
    async fn handle(&self, context: Context, event: &Event) -> Result<(), HandlerError> {
        'routes: for route in &self.routes {
            let filters_match = std::panic::catch_unwind(AssertUnwindSafe(|| {
                route
                    .filters
                    .iter()
                    .all(|filter| filter.matches(&context, event))
            }))
            .map_err(|_| HandlerError::Failed("route filter panicked".to_owned()))?;
            if !filters_match {
                continue;
            }
            let mut entered = EnteredMiddlewareStack::new();
            for middleware in &route.middleware {
                let entering = EnteredMiddleware::new(middleware, &context, event);
                let before = catch_async_panic(
                    async { entering.middleware.before(&context, event).await },
                    "route middleware before hook",
                )
                .await;
                match before {
                    Ok(result) => match result {
                        Ok(MiddlewareControl::Continue) => entered.push(entering),
                        Ok(MiddlewareControl::Stop) => {
                            entered.push(entering);
                            if let Some(error) =
                                run_after(entered, &context, event, RouteOutcome::Skipped).await
                            {
                                tracing::error!(
                                    handler = handler_name(&route.handler),
                                    priority = route.priority,
                                    error = %error,
                                    "route middleware cleanup failed after a stop"
                                );
                                return Err(error);
                            }
                            if route.stop_after_match {
                                break 'routes;
                            }
                            continue 'routes;
                        }
                        Err(error) => {
                            call_failed_hook(entering.middleware, &context, event);
                            let mut entering = entering;
                            entering.completed = true;
                            drop(entering);
                            tracing::error!(
                                handler = handler_name(&route.handler),
                                priority = route.priority,
                                error = %error,
                                "route middleware before hook failed"
                            );
                            if let Some(after_error) =
                                run_after(entered, &context, event, RouteOutcome::Failure).await
                            {
                                tracing::error!(
                                    handler = handler_name(&route.handler),
                                    priority = route.priority,
                                    error = %after_error,
                                    "route middleware cleanup failed after a before hook error"
                                );
                            }
                            return Err(error);
                        }
                    },
                    Err(error) => {
                        call_failed_hook(entering.middleware, &context, event);
                        let mut entering = entering;
                        entering.completed = true;
                        drop(entering);
                        if let Some(after_error) =
                            run_after(entered, &context, event, RouteOutcome::Failure).await
                        {
                            tracing::error!(
                                handler = handler_name(&route.handler),
                                priority = route.priority,
                                error = %after_error,
                                "route middleware cleanup failed after a before hook panic"
                            );
                        }
                        return Err(error);
                    }
                }
            }
            let result = catch_async_panic(
                async { route.handler.handle(context.clone(), event).await },
                "route handler",
            )
            .await
            .and_then(std::convert::identity);
            let outcome = if result.is_ok() {
                RouteOutcome::Success
            } else {
                RouteOutcome::Failure
            };
            let middleware_error = run_after(entered, &context, event, outcome).await;
            match result {
                Ok(()) => {
                    if let Some(error) = middleware_error {
                        tracing::error!(
                            handler = handler_name(&route.handler),
                            priority = route.priority,
                            error = %error,
                            "route middleware after hook failed"
                        );
                        return Err(error);
                    }
                }
                Err(error) => {
                    tracing::error!(
                        handler = handler_name(&route.handler),
                        priority = route.priority,
                        error = %error,
                        "route handler failed"
                    );
                    if let Some(after_error) = middleware_error {
                        tracing::error!(
                            handler = handler_name(&route.handler),
                            priority = route.priority,
                            error = %after_error,
                            "route middleware cleanup failed after a handler error"
                        );
                    }
                    return Err(error);
                }
            }
            if route.stop_after_match {
                break;
            }
        }
        Ok(())
    }
}

struct EnteredMiddleware<'a> {
    middleware: &'a Arc<dyn EventMiddleware>,
    context: &'a Context,
    event: &'a Event,
    completed: bool,
}

struct EnteredMiddlewareStack<'a> {
    entered: Vec<EnteredMiddleware<'a>>,
}

impl<'a> EnteredMiddlewareStack<'a> {
    const fn new() -> Self {
        Self {
            entered: Vec::new(),
        }
    }

    fn push(&mut self, entered: EnteredMiddleware<'a>) {
        self.entered.push(entered);
    }

    fn pop(&mut self) -> Option<EnteredMiddleware<'a>> {
        self.entered.pop()
    }
}

impl Drop for EnteredMiddlewareStack<'_> {
    fn drop(&mut self) {
        while let Some(entered) = self.entered.pop() {
            drop(entered);
        }
    }
}

impl<'a> EnteredMiddleware<'a> {
    fn new(
        middleware: &'a Arc<dyn EventMiddleware>,
        context: &'a Context,
        event: &'a Event,
    ) -> Self {
        Self {
            middleware,
            context,
            event,
            completed: false,
        }
    }
}

impl Drop for EnteredMiddleware<'_> {
    fn drop(&mut self) {
        if !self.completed {
            let cancelled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.middleware.cancelled(self.context, self.event);
            }));
            if cancelled.is_err() {
                tracing::error!("route middleware cancellation hook panicked");
            }
        }
    }
}

async fn run_after(
    mut entered: EnteredMiddlewareStack<'_>,
    context: &Context,
    event: &Event,
    outcome: RouteOutcome,
) -> Option<HandlerError> {
    let mut first_error = None;
    while let Some(mut entered) = entered.pop() {
        let result = catch_async_panic(
            async { entered.middleware.after(context, event, outcome).await },
            "route middleware after hook",
        )
        .await
        .and_then(std::convert::identity);
        entered.completed = true;
        if let Err(error) = result {
            if first_error.is_some() {
                tracing::error!(
                    error = %error,
                    "additional route middleware cleanup hook failed"
                );
            } else {
                first_error = Some(error);
            }
        }
    }
    first_error
}

async fn catch_async_panic<T>(
    future: impl Future<Output = T>,
    operation: &'static str,
) -> Result<T, HandlerError> {
    AssertUnwindSafe(future)
        .catch_unwind()
        .await
        .map_err(|_| HandlerError::Failed(format!("{operation} panicked")))
}

fn call_failed_hook(middleware: &Arc<dyn EventMiddleware>, context: &Context, event: &Event) {
    if std::panic::catch_unwind(AssertUnwindSafe(|| middleware.failed(context, event))).is_err() {
        tracing::error!("route middleware failed hook panicked");
    }
}

fn handler_name(handler: &Arc<dyn EventHandler>) -> String {
    std::panic::catch_unwind(AssertUnwindSafe(|| handler.name().to_owned()))
        .unwrap_or_else(|_| "<panicking-handler-name>".to_owned())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    Message,
    Notice,
    Request,
    Lifecycle,
    Platform,
}

#[derive(Debug)]
pub struct EventKindFilter(pub EventKind);

impl EventFilter for EventKindFilter {
    fn matches(&self, _context: &Context, event: &Event) -> bool {
        matches!(
            (self.0, event),
            (EventKind::Message, Event::Message(_))
                | (EventKind::Notice, Event::Notice(_))
                | (EventKind::Request, Event::Request(_))
                | (EventKind::Lifecycle, Event::Lifecycle(_))
                | (EventKind::Platform, Event::Platform { .. })
        )
    }
}

#[derive(Debug)]
pub struct MessageScopeFilter(pub MessageScope);

impl EventFilter for MessageScopeFilter {
    fn matches(&self, _context: &Context, event: &Event) -> bool {
        matches!(event, Event::Message(message) if message.scope() == self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandInvocation {
    pub name: String,
    pub arguments: String,
    pub raw: String,
}

#[async_trait]
pub trait CommandHandler: Send + Sync + 'static {
    async fn handle(
        &self,
        context: Context,
        invocation: &CommandInvocation,
    ) -> Result<(), HandlerError>;
}

struct RegisteredCommand {
    canonical_name: String,
    handler: Arc<dyn CommandHandler>,
}

impl std::fmt::Debug for RegisteredCommand {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RegisteredCommand")
            .field("canonical_name", &self.canonical_name)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Error)]
pub enum RouterError {
    #[error("command name `{0}` is invalid")]
    InvalidCommand(String),
    #[error("command name or alias `{0}` is already registered")]
    DuplicateCommand(String),
    #[error("event router cannot contain more than {MAX_EVENT_ROUTES} routes")]
    TooManyRoutes,
    #[error("an event route cannot contain more than {MAX_ROUTE_FILTERS} filters")]
    TooManyRouteFilters,
    #[error("an event route cannot contain more than {MAX_ROUTE_MIDDLEWARE} middleware hooks")]
    TooManyRouteMiddleware,
    #[error("command router cannot contain more than {MAX_COMMAND_KEYS} command names and aliases")]
    TooManyCommands,
    #[error("command router cannot contain more than {MAX_COMMAND_PREFIXES} prefixes")]
    TooManyPrefixes,
    #[error("command router cannot contain more than {MAX_COMMAND_MENTIONS} mentions")]
    TooManyMentions,
    #[error("command {kind} `{value}` is empty, oversized, or contains control characters")]
    InvalidCommandTrigger { kind: &'static str, value: String },
}

#[derive(Debug)]
pub struct CommandRouter {
    name: String,
    prefixes: Vec<String>,
    mentions: Vec<String>,
    commands: BTreeMap<String, RegisteredCommand>,
}

impl Default for CommandRouter {
    fn default() -> Self {
        Self {
            name: "command-router".to_owned(),
            prefixes: vec!["/".to_owned()],
            mentions: Vec::new(),
            commands: BTreeMap::new(),
        }
    }
}

impl CommandRouter {
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    pub fn prefixes(
        mut self,
        prefixes: impl IntoIterator<Item = String>,
    ) -> Result<Self, RouterError> {
        self.prefixes = collect_command_triggers(prefixes, "prefix", MAX_COMMAND_PREFIXES)?;
        self.prefixes.sort_by_key(|prefix| Reverse(prefix.len()));
        Ok(self)
    }

    pub fn mentions(
        mut self,
        mentions: impl IntoIterator<Item = String>,
    ) -> Result<Self, RouterError> {
        self.mentions = collect_command_triggers(mentions, "mention", MAX_COMMAND_MENTIONS)?;
        self.mentions.sort_by_key(|mention| Reverse(mention.len()));
        Ok(self)
    }

    #[allow(clippy::needless_pass_by_value)]
    pub fn command(
        mut self,
        name: impl Into<String>,
        aliases: impl IntoIterator<Item = String>,
        handler: Arc<dyn CommandHandler>,
    ) -> Result<Self, RouterError> {
        let name = normalize_command(&name.into())?;
        if self.commands.len() >= MAX_COMMAND_KEYS {
            return Err(RouterError::TooManyCommands);
        }
        let mut normalized_aliases = Vec::new();
        for alias in aliases {
            if self
                .commands
                .len()
                .saturating_add(normalized_aliases.len())
                .saturating_add(2)
                > MAX_COMMAND_KEYS
            {
                return Err(RouterError::TooManyCommands);
            }
            normalized_aliases.push(normalize_command(&alias)?);
        }
        let keys = std::iter::once(name.clone())
            .chain(normalized_aliases)
            .collect::<Vec<_>>();
        let mut unique = BTreeSet::new();
        for key in &keys {
            if !unique.insert(key.clone()) || self.commands.contains_key(key) {
                return Err(RouterError::DuplicateCommand(key.clone()));
            }
        }
        for key in keys {
            self.commands.insert(
                key,
                RegisteredCommand {
                    canonical_name: name.clone(),
                    handler: handler.clone(),
                },
            );
        }
        Ok(self)
    }

    fn parse(&self, text: &str) -> Option<CommandInvocation> {
        if text.len() > MAX_COMMAND_TEXT_BYTES {
            return None;
        }
        let original_text = text;
        let mut text = text.trim();
        if let Some(mention) = self.mentions.iter().find(|mention| {
            text.strip_prefix(mention.as_str())
                .is_some_and(|remaining| {
                    remaining.is_empty() || remaining.starts_with(char::is_whitespace)
                })
        }) {
            text = text[mention.len()..].trim_start();
        }
        let prefix = self
            .prefixes
            .iter()
            .find(|prefix| text.starts_with(prefix.as_str()))?;
        let command = text[prefix.len()..].trim_start();
        let (name, arguments) = command
            .split_once(char::is_whitespace)
            .map_or((command, ""), |(name, arguments)| (name, arguments.trim()));
        let registered = self.commands.get(&name.to_ascii_lowercase())?;
        Some(CommandInvocation {
            name: registered.canonical_name.clone(),
            arguments: arguments.to_owned(),
            raw: original_text.to_owned(),
        })
    }
}

#[async_trait]
impl EventHandler for CommandRouter {
    fn name(&self) -> &str {
        &self.name
    }

    async fn handle(&self, context: Context, event: &Event) -> Result<(), HandlerError> {
        let Event::Message(message) = event else {
            return Ok(());
        };
        let Some(invocation) = self.parse(&message.text) else {
            return Ok(());
        };
        let Some(command) = self.commands.get(&invocation.name) else {
            return Ok(());
        };
        catch_async_panic(
            async { command.handler.handle(context, &invocation).await },
            "command handler",
        )
        .await
        .and_then(std::convert::identity)
    }
}

fn normalize_command(value: &str) -> Result<String, RouterError> {
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty()
        || value.len() > MAX_COMMAND_NAME_BYTES
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(RouterError::InvalidCommand(value));
    }
    Ok(value)
}

fn collect_command_triggers(
    values: impl IntoIterator<Item = String>,
    kind: &'static str,
    maximum: usize,
) -> Result<Vec<String>, RouterError> {
    let mut collected = Vec::new();
    for value in values {
        if collected.len() >= maximum {
            return Err(match kind {
                "prefix" => RouterError::TooManyPrefixes,
                _ => RouterError::TooManyMentions,
            });
        }
        if value.trim().is_empty()
            || value != value.trim()
            || value.len() > MAX_COMMAND_TRIGGER_BYTES
            || value.chars().any(char::is_control)
        {
            return Err(RouterError::InvalidCommandTrigger { kind, value });
        }
        collected.push(value);
    }
    Ok(collected)
}

#[cfg(test)]
mod tests {
    use std::{
        future::{Future as _, poll_fn},
        sync::{Arc, Mutex},
        task::Poll,
        time::Duration,
    };

    use async_trait::async_trait;
    use serde_json::{Value, json};

    use super::{
        CommandHandler, CommandInvocation, CommandRouter, EventFilter, EventMiddleware, EventRoute,
        EventRouter, MiddlewareControl, RouteOutcome, RouterError,
    };
    use crate::{
        Action, ActionResult, Adapter, AdapterError, AdapterId, CommonMessage, Context, Event,
        EventEnvelope, EventHandler, EventId, EventSender, HandlerError, MessageTarget, Sender,
        ShutdownSignal,
    };

    #[derive(Debug)]
    struct NoopAdapter {
        id: AdapterId,
    }

    #[async_trait]
    impl Adapter for NoopAdapter {
        fn id(&self) -> &AdapterId {
            &self.id
        }

        fn platform(&self) -> &'static str {
            "test"
        }

        async fn run(
            &self,
            _events: EventSender,
            mut shutdown: ShutdownSignal,
        ) -> Result<(), AdapterError> {
            shutdown.cancelled().await;
            Ok(())
        }

        async fn execute(&self, _action: Action) -> Result<ActionResult, AdapterError> {
            Ok(ActionResult {
                message_id: None,
                raw: Value::Null,
            })
        }
    }

    fn message(text: &str) -> EventEnvelope {
        EventEnvelope {
            id: EventId::new("route-event"),
            adapter: AdapterId::new("router-test"),
            delivery_id: None,
            timestamp: None,
            event: Event::Message(CommonMessage {
                message_id: "message".to_owned(),
                target: MessageTarget::Group {
                    group_id: "group".to_owned(),
                },
                sender: Sender {
                    id: "user".to_owned(),
                    display_name: None,
                },
                text: text.to_owned(),
                segments: Vec::new(),
                reply_to: None,
            }),
            raw: json!({"text":text}),
        }
    }

    fn context(event: &EventEnvelope) -> Context {
        Context::new(
            event,
            Arc::new(NoopAdapter {
                id: event.adapter.clone(),
            }),
        )
    }

    #[derive(Debug, Default)]
    struct Recorder(Mutex<Vec<CommandInvocation>>);

    #[async_trait]
    impl CommandHandler for Recorder {
        async fn handle(
            &self,
            _context: Context,
            invocation: &CommandInvocation,
        ) -> Result<(), HandlerError> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(invocation.clone());
            Ok(())
        }
    }

    #[derive(Debug)]
    struct PanickingCommandHandler;

    #[async_trait]
    impl CommandHandler for PanickingCommandHandler {
        async fn handle(
            &self,
            _context: Context,
            _invocation: &CommandInvocation,
        ) -> Result<(), HandlerError> {
            panic!("expected command handler panic");
        }
    }

    #[derive(Debug)]
    struct RecordingHandler {
        name: &'static str,
        calls: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl EventHandler for RecordingHandler {
        fn name(&self) -> &'static str {
            self.name
        }

        async fn handle(&self, _context: Context, _event: &Event) -> Result<(), HandlerError> {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(format!("handler:{}", self.name));
            Ok(())
        }
    }

    #[derive(Debug)]
    struct FailingHandler;

    #[async_trait]
    impl EventHandler for FailingHandler {
        fn name(&self) -> &'static str {
            "failing"
        }

        async fn handle(&self, _context: Context, _event: &Event) -> Result<(), HandlerError> {
            Err(HandlerError::Failed("expected route failure".to_owned()))
        }
    }

    #[derive(Debug)]
    struct PendingHandler;

    #[async_trait]
    impl EventHandler for PendingHandler {
        fn name(&self) -> &'static str {
            "pending"
        }

        async fn handle(&self, _context: Context, _event: &Event) -> Result<(), HandlerError> {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(())
        }
    }

    #[derive(Debug)]
    struct TextFilter(&'static str);

    impl EventFilter for TextFilter {
        fn matches(&self, _context: &Context, event: &Event) -> bool {
            matches!(event, Event::Message(message) if message.text == self.0)
        }
    }

    #[derive(Debug)]
    struct RecordingMiddleware {
        name: &'static str,
        calls: Arc<Mutex<Vec<String>>>,
        control: MiddlewareControl,
    }

    #[async_trait]
    impl EventMiddleware for RecordingMiddleware {
        async fn before(
            &self,
            _context: &Context,
            _event: &Event,
        ) -> Result<MiddlewareControl, HandlerError> {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(format!("before:{}", self.name));
            Ok(self.control)
        }

        async fn after(
            &self,
            _context: &Context,
            _event: &Event,
            outcome: RouteOutcome,
        ) -> Result<(), HandlerError> {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(format!("after:{}:{outcome:?}", self.name));
            Ok(())
        }

        fn cancelled(&self, _context: &Context, _event: &Event) {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(format!("cancelled:{}", self.name));
        }
    }

    #[derive(Debug)]
    struct PendingBeforeMiddleware {
        calls: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl EventMiddleware for PendingBeforeMiddleware {
        async fn before(
            &self,
            _context: &Context,
            _event: &Event,
        ) -> Result<MiddlewareControl, HandlerError> {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push("before:pending".to_owned());
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(MiddlewareControl::Continue)
        }

        fn cancelled(&self, _context: &Context, _event: &Event) {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push("cancelled:pending".to_owned());
        }
    }

    #[derive(Debug)]
    struct PendingAfterMiddleware {
        name: &'static str,
        calls: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl EventMiddleware for PendingAfterMiddleware {
        async fn before(
            &self,
            _context: &Context,
            _event: &Event,
        ) -> Result<MiddlewareControl, HandlerError> {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(format!("before:{}", self.name));
            Ok(MiddlewareControl::Continue)
        }

        async fn after(
            &self,
            _context: &Context,
            _event: &Event,
            outcome: RouteOutcome,
        ) -> Result<(), HandlerError> {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(format!("after:{}:{outcome:?}", self.name));
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(())
        }

        fn cancelled(&self, _context: &Context, _event: &Event) {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(format!("cancelled:{}", self.name));
        }
    }

    #[derive(Debug)]
    struct FailingBeforeMiddleware {
        calls: Arc<Mutex<Vec<String>>>,
    }

    #[derive(Debug)]
    struct PanickingCancelledMiddleware;

    #[derive(Debug)]
    struct PanickingFilter;

    impl EventFilter for PanickingFilter {
        fn matches(&self, _context: &Context, _event: &Event) -> bool {
            panic!("expected filter panic");
        }
    }

    #[derive(Debug)]
    struct PanickingHandler;

    #[async_trait]
    impl EventHandler for PanickingHandler {
        fn name(&self) -> &'static str {
            panic!("expected handler name panic");
        }

        async fn handle(&self, _context: Context, _event: &Event) -> Result<(), HandlerError> {
            panic!("expected handler panic");
        }
    }

    #[derive(Debug)]
    struct PanickingMiddleware {
        panic_before: bool,
    }

    #[async_trait]
    impl EventMiddleware for PanickingMiddleware {
        async fn before(
            &self,
            _context: &Context,
            _event: &Event,
        ) -> Result<MiddlewareControl, HandlerError> {
            assert!(!self.panic_before, "expected before panic");
            Ok(MiddlewareControl::Continue)
        }

        async fn after(
            &self,
            _context: &Context,
            _event: &Event,
            _outcome: RouteOutcome,
        ) -> Result<(), HandlerError> {
            assert!(self.panic_before, "expected after panic");
            Ok(())
        }

        fn failed(&self, _context: &Context, _event: &Event) {
            assert!(!self.panic_before, "expected failed hook panic");
        }
    }

    #[async_trait]
    impl EventMiddleware for PanickingCancelledMiddleware {
        fn cancelled(&self, _context: &Context, _event: &Event) {
            panic!("expected cancellation hook panic");
        }
    }

    #[async_trait]
    impl EventMiddleware for FailingBeforeMiddleware {
        async fn before(
            &self,
            _context: &Context,
            _event: &Event,
        ) -> Result<MiddlewareControl, HandlerError> {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push("before:failing".to_owned());
            Err(HandlerError::Failed("expected setup failure".to_owned()))
        }

        fn failed(&self, _context: &Context, _event: &Event) {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push("failed:failing".to_owned());
        }

        fn cancelled(&self, _context: &Context, _event: &Event) {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push("cancelled:failing".to_owned());
        }
    }

    #[test]
    fn command_parser_supports_prefix_mentions_aliases_and_arguments() {
        let handler = Arc::new(Recorder::default());
        let router = CommandRouter::new()
            .mentions(["@bot".to_owned()])
            .unwrap()
            .prefixes(["/".to_owned(), "//".to_owned()])
            .unwrap()
            .command("ping", ["p".to_owned()], handler)
            .unwrap();
        let invocation = router.parse(" @bot /P hello world ").unwrap();
        assert_eq!(invocation.name, "ping");
        assert_eq!(invocation.arguments, "hello world");
        assert_eq!(router.parse("//ping").unwrap().name, "ping");
        assert!(router.parse("/pingpong").is_none());
        assert!(router.parse("@botany /ping").is_none());
        assert!(
            router
                .parse(&format!(
                    "/ping {}",
                    "x".repeat(super::MAX_COMMAND_TEXT_BYTES)
                ))
                .is_none()
        );

        assert!(matches!(
            CommandRouter::new().prefixes([" ".to_owned()]),
            Err(RouterError::InvalidCommandTrigger { kind: "prefix", .. })
        ));
        assert!(matches!(
            CommandRouter::new().mentions(["\t".to_owned()]),
            Err(RouterError::InvalidCommandTrigger {
                kind: "mention",
                ..
            })
        ));
        assert!(matches!(
            CommandRouter::new().prefixes([" /".to_owned()]),
            Err(RouterError::InvalidCommandTrigger { kind: "prefix", .. })
        ));
        assert!(matches!(
            CommandRouter::new().mentions(["@bot ".to_owned()]),
            Err(RouterError::InvalidCommandTrigger {
                kind: "mention",
                ..
            })
        ));
    }

    #[test]
    fn command_router_rejects_duplicate_aliases_within_registration() {
        let result = CommandRouter::new().command(
            "ping",
            ["p".to_owned(), "P".to_owned()],
            Arc::new(Recorder::default()),
        );
        assert!(matches!(result, Err(RouterError::DuplicateCommand(alias)) if alias == "p"));
    }

    #[tokio::test]
    async fn command_router_converts_handler_panics_to_errors() {
        let router = CommandRouter::new()
            .prefixes(["/".to_owned()])
            .unwrap()
            .command(
                "panic",
                Vec::<String>::new(),
                Arc::new(PanickingCommandHandler),
            )
            .unwrap();
        let event = message("/panic");
        assert!(router.handle(context(&event), &event.event).await.is_err());
    }

    #[test]
    fn router_registration_limits_bound_retained_work() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let handler = Arc::new(RecordingHandler {
            name: "bounded",
            calls: Arc::clone(&calls),
        });
        let mut router = EventRouter::new();
        for _ in 0..super::MAX_EVENT_ROUTES {
            router = router.route(EventRoute::new(handler.clone())).unwrap();
        }
        assert!(matches!(
            router.route(EventRoute::new(handler.clone())),
            Err(RouterError::TooManyRoutes)
        ));

        let mut route = EventRoute::new(handler.clone());
        for _ in 0..super::MAX_ROUTE_FILTERS {
            route = route.filter(Arc::new(TextFilter("bounded"))).unwrap();
        }
        assert!(matches!(
            route.filter(Arc::new(TextFilter("overflow"))),
            Err(RouterError::TooManyRouteFilters)
        ));

        let middleware = Arc::new(RecordingMiddleware {
            name: "bounded",
            calls,
            control: MiddlewareControl::Continue,
        });
        let mut route = EventRoute::new(handler);
        for _ in 0..super::MAX_ROUTE_MIDDLEWARE {
            route = route.middleware(middleware.clone()).unwrap();
        }
        assert!(matches!(
            route.middleware(middleware),
            Err(RouterError::TooManyRouteMiddleware)
        ));

        assert!(matches!(
            CommandRouter::new()
                .prefixes((0..=super::MAX_COMMAND_PREFIXES).map(|index| format!("/{index}"))),
            Err(RouterError::TooManyPrefixes)
        ));
        assert!(matches!(
            CommandRouter::new().command(
                "root",
                (0..super::MAX_COMMAND_KEYS).map(|index| format!("alias{index}")),
                Arc::new(Recorder::default()),
            ),
            Err(RouterError::TooManyCommands)
        ));
        assert!(matches!(
            CommandRouter::new().command(
                "x".repeat(super::MAX_COMMAND_NAME_BYTES + 1),
                Vec::new(),
                Arc::new(Recorder::default()),
            ),
            Err(RouterError::InvalidCommand(_))
        ));
    }

    #[tokio::test]
    async fn event_router_applies_filters_priority_middleware_and_stop() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let handler = |name| {
            Arc::new(RecordingHandler {
                name,
                calls: Arc::clone(&calls),
            }) as Arc<dyn EventHandler>
        };
        let middleware = |name| {
            Arc::new(RecordingMiddleware {
                name,
                calls: Arc::clone(&calls),
                control: MiddlewareControl::Continue,
            }) as Arc<dyn EventMiddleware>
        };
        let later = EventRoute::new(handler("later"))
            .priority(10)
            .filter(Arc::new(TextFilter("/ping")))
            .unwrap();
        let filtered = EventRoute::new(handler("filtered"))
            .priority(-20)
            .filter(Arc::new(TextFilter("no-match")))
            .unwrap();
        let first = EventRoute::new(handler("first"))
            .priority(-10)
            .middleware(middleware("outer"))
            .unwrap()
            .middleware(middleware("inner"))
            .unwrap()
            .stop_after_match(true);
        let router = EventRouter::new()
            .route(later)
            .unwrap()
            .route(filtered)
            .unwrap()
            .route(first)
            .unwrap();
        let event = message("/ping");
        router.handle(context(&event), &event.event).await.unwrap();
        assert_eq!(
            *calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            [
                "before:outer",
                "before:inner",
                "handler:first",
                "after:inner:Success",
                "after:outer:Success",
            ]
        );
    }

    #[tokio::test]
    async fn event_router_unwinds_entered_middleware_when_route_is_stopped() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let middleware = |name, control| {
            Arc::new(RecordingMiddleware {
                name,
                calls: Arc::clone(&calls),
                control,
            }) as Arc<dyn EventMiddleware>
        };
        let stopped = EventRoute::new(Arc::new(RecordingHandler {
            name: "must-not-run",
            calls: Arc::clone(&calls),
        }))
        .middleware(middleware("outer", MiddlewareControl::Continue))
        .unwrap()
        .middleware(middleware("stopper", MiddlewareControl::Stop))
        .unwrap()
        .stop_after_match(true);
        let router = EventRouter::new()
            .route(stopped)
            .unwrap()
            .route(EventRoute::new(Arc::new(RecordingHandler {
                name: "later",
                calls: Arc::clone(&calls),
            })))
            .unwrap();
        let event = message("/ping");
        router.handle(context(&event), &event.event).await.unwrap();
        assert_eq!(
            *calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            [
                "before:outer",
                "before:stopper",
                "after:stopper:Skipped",
                "after:outer:Skipped",
            ]
        );
    }

    #[tokio::test]
    async fn event_router_stops_after_a_route_handler_failure() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let router = EventRouter::new()
            .route(EventRoute::new(Arc::new(FailingHandler)))
            .unwrap()
            .route(EventRoute::new(Arc::new(RecordingHandler {
                name: "later",
                calls: Arc::clone(&calls),
            })))
            .unwrap();
        let event = message("/ping");
        assert!(router.handle(context(&event), &event.event).await.is_err());
        assert!(
            calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );
    }

    #[tokio::test]
    async fn event_router_converts_user_panics_to_handler_errors() {
        let event = message("/ping");
        let filter_router = EventRouter::new()
            .route(
                EventRoute::new(Arc::new(RecordingHandler {
                    name: "unused",
                    calls: Arc::new(Mutex::new(Vec::new())),
                }))
                .filter(Arc::new(PanickingFilter))
                .unwrap(),
            )
            .unwrap();
        assert!(
            filter_router
                .handle(context(&event), &event.event)
                .await
                .is_err()
        );

        let handler_router = EventRouter::new()
            .route(EventRoute::new(Arc::new(PanickingHandler)))
            .unwrap();
        assert!(
            handler_router
                .handle(context(&event), &event.event)
                .await
                .is_err()
        );

        for panic_before in [true, false] {
            let middleware_router = EventRouter::new()
                .route(
                    EventRoute::new(Arc::new(RecordingHandler {
                        name: "middleware-panic",
                        calls: Arc::new(Mutex::new(Vec::new())),
                    }))
                    .middleware(Arc::new(PanickingMiddleware { panic_before }))
                    .unwrap(),
                )
                .unwrap();
            assert!(
                middleware_router
                    .handle(context(&event), &event.event)
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn event_router_runs_synchronous_cleanup_when_cancelled() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let middleware = |name| {
            Arc::new(RecordingMiddleware {
                name,
                calls: Arc::clone(&calls),
                control: MiddlewareControl::Continue,
            })
        };
        let route = EventRoute::new(Arc::new(PendingHandler))
            .middleware(middleware("outer"))
            .unwrap()
            .middleware(middleware("inner"))
            .unwrap();
        let router = EventRouter::new().route(route).unwrap();
        let event = message("/ping");
        let event_context = context(&event);
        let mut handling = Box::pin(router.handle(event_context, &event.event));
        poll_fn(|context| match handling.as_mut().poll(context) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(_) => panic!("pending route unexpectedly completed"),
        })
        .await;
        drop(handling);

        assert_eq!(
            *calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            [
                "before:outer",
                "before:inner",
                "cancelled:inner",
                "cancelled:outer",
            ]
        );
    }

    #[tokio::test]
    async fn event_router_contains_panics_from_cancellation_hooks() {
        let route = EventRoute::new(Arc::new(PendingHandler))
            .middleware(Arc::new(PanickingCancelledMiddleware))
            .unwrap();
        let router = EventRouter::new().route(route).unwrap();
        let event = message("/ping");
        let event_context = context(&event);
        let mut handling = Box::pin(router.handle(event_context, &event.event));
        poll_fn(|context| match handling.as_mut().poll(context) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(_) => panic!("pending route unexpectedly completed"),
        })
        .await;

        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(handling))).is_ok());
    }

    #[tokio::test]
    async fn event_router_guards_cancellation_during_before_hook() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let route = EventRoute::new(Arc::new(RecordingHandler {
            name: "must-not-run",
            calls: Arc::clone(&calls),
        }))
        .middleware(Arc::new(PendingBeforeMiddleware {
            calls: Arc::clone(&calls),
        }))
        .unwrap();
        let router = EventRouter::new().route(route).unwrap();
        let event = message("/ping");
        let event_context = context(&event);
        let mut handling = Box::pin(router.handle(event_context, &event.event));
        poll_fn(|context| match handling.as_mut().poll(context) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(_) => panic!("pending before hook unexpectedly completed"),
        })
        .await;
        drop(handling);

        assert_eq!(
            *calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            ["before:pending", "cancelled:pending"]
        );
    }

    #[tokio::test]
    async fn event_router_unwinds_only_incomplete_after_hooks_when_cancelled() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recording = |name| {
            Arc::new(RecordingMiddleware {
                name,
                calls: Arc::clone(&calls),
                control: MiddlewareControl::Continue,
            }) as Arc<dyn EventMiddleware>
        };
        let route = EventRoute::new(Arc::new(RecordingHandler {
            name: "handler",
            calls: Arc::clone(&calls),
        }))
        .middleware(recording("outer"))
        .unwrap()
        .middleware(Arc::new(PendingAfterMiddleware {
            name: "pending",
            calls: Arc::clone(&calls),
        }))
        .unwrap()
        .middleware(recording("inner"))
        .unwrap();
        let router = EventRouter::new().route(route).unwrap();
        let event = message("/ping");
        let event_context = context(&event);
        let mut handling = Box::pin(router.handle(event_context, &event.event));
        poll_fn(|context| match handling.as_mut().poll(context) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(_) => panic!("pending after hook unexpectedly completed"),
        })
        .await;
        drop(handling);

        assert_eq!(
            *calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            [
                "before:outer",
                "before:pending",
                "before:inner",
                "handler:handler",
                "after:inner:Success",
                "after:pending:Success",
                "cancelled:pending",
                "cancelled:outer",
            ]
        );
    }

    #[tokio::test]
    async fn event_router_distinguishes_before_failure_from_cancellation() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let route = EventRoute::new(Arc::new(RecordingHandler {
            name: "must-not-run",
            calls: Arc::clone(&calls),
        }))
        .middleware(Arc::new(RecordingMiddleware {
            name: "outer",
            calls: Arc::clone(&calls),
            control: MiddlewareControl::Continue,
        }))
        .unwrap()
        .middleware(Arc::new(FailingBeforeMiddleware {
            calls: Arc::clone(&calls),
        }))
        .unwrap();
        let router = EventRouter::new().route(route).unwrap();
        let event = message("/ping");
        assert!(router.handle(context(&event), &event.event).await.is_err());
        assert_eq!(
            *calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            [
                "before:outer",
                "before:failing",
                "failed:failing",
                "after:outer:Failure",
            ]
        );
    }

    #[tokio::test]
    async fn command_router_dispatches_alias_as_canonical_command() {
        let recorder = Arc::new(Recorder::default());
        let router = CommandRouter::new()
            .mentions(["@bot".to_owned()])
            .unwrap()
            .command("ping", ["p".to_owned()], recorder.clone())
            .unwrap();
        let event = message("@bot /P hello world");
        router.handle(context(&event), &event.event).await.unwrap();
        assert_eq!(
            *recorder
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            [CommandInvocation {
                name: "ping".to_owned(),
                arguments: "hello world".to_owned(),
                raw: "@bot /P hello world".to_owned(),
            }]
        );
    }
}
