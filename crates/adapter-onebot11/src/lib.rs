//! `OneBot` 11 reverse WebSocket adapter for `bot-core`.

#![forbid(unsafe_code)]

use std::{
    collections::{HashMap, VecDeque},
    net::SocketAddr,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use bot_core::{
    Action, ActionResult, Adapter, AdapterError, AdapterId, EventEnvelope, EventSendError,
    EventSender, MediaAttachment, MessageTarget, ShutdownSignal,
};
use futures_util::{Sink, SinkExt as _, StreamExt as _};
use onebot_protocol::{ActionRequest, ActionResponse, MessageSegment, OneBotId, response_like};
use secrecy::{ExposeSecret as _, SecretString};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq as _;
use tokio::{
    io::AsyncWriteExt as _,
    net::{TcpListener, TcpStream},
    sync::{Mutex, OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
    task::{JoinHandle, JoinSet},
    time::{Instant, interval, timeout},
};
use tokio_tungstenite::{
    WebSocketStream, accept_hdr_async_with_config,
    tungstenite::{
        Message as WebSocketMessage, Utf8Bytes, handshake::server::ErrorResponse, http::StatusCode,
        protocol::WebSocketConfig,
    },
};

const MAX_INLINE_MEDIA_BYTES: usize = 8 * 1024 * 1024;
const MAX_QUEUED_INLINE_MEDIA_BYTES: usize = 16 * 1024 * 1024;
const MAX_CONCURRENT_INLINE_MEDIA: usize = 8;
use tracing::{debug, info, warn};
use uuid::Uuid;

mod mapping;

pub use mapping::{MappingError, map_event};

const MAX_PENDING_HANDSHAKES: usize = 8;
pub const MAX_PENDING_EVENTS_PER_CONNECTION: usize = 64;

enum TextHandling {
    Continue,
}

#[derive(Clone)]
pub struct OneBot11Config {
    pub id: AdapterId,
    pub listen: SocketAddr,
    pub access_token: SecretString,
    pub allow_insecure_remote: bool,
    pub action_timeout: Duration,
    pub max_message_bytes: usize,
    pub max_pending_actions: usize,
}

impl std::fmt::Debug for OneBot11Config {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OneBot11Config")
            .field("id", &self.id)
            .field("listen", &self.listen)
            .field("access_token", &"[REDACTED]")
            .field("allow_insecure_remote", &self.allow_insecure_remote)
            .field("action_timeout", &self.action_timeout)
            .field("max_message_bytes", &self.max_message_bytes)
            .field("max_pending_actions", &self.max_pending_actions)
            .finish()
    }
}

impl Default for OneBot11Config {
    fn default() -> Self {
        Self {
            id: AdapterId::new("onebot11-reverse"),
            listen: SocketAddr::from(([127, 0, 0, 1], 6700)),
            access_token: SecretString::from(String::new()),
            allow_insecure_remote: false,
            action_timeout: Duration::from_secs(15),
            max_message_bytes: 1024 * 1024,
            max_pending_actions: 256,
        }
    }
}

struct OutboundAction {
    action: String,
    params: Value,
    deadline: Instant,
    response: oneshot::Sender<Result<ActionResponse, AdapterError>>,
    media_budget: Option<OwnedSemaphorePermit>,
    media_slot: Option<OwnedSemaphorePermit>,
}

struct PendingAction {
    deadline: Instant,
    response: oneshot::Sender<Result<ActionResponse, AdapterError>>,
    _media_budget: Option<OwnedSemaphorePermit>,
    _media_slot: Option<OwnedSemaphorePermit>,
}

#[derive(Default)]
struct ConnectionState {
    generation: u64,
    outbound: Option<mpsc::Sender<OutboundAction>>,
    event_queue_full: bool,
    event_producer_waiting: bool,
}

struct ConnectionGuard {
    connection: Arc<StdMutex<ConnectionState>>,
    generation: u64,
    events: EventSender,
    active_slot: Arc<AtomicBool>,
    forwarder_abort: tokio::task::AbortHandle,
    unpublished: bool,
}

impl ConnectionGuard {
    fn unpublish(&mut self) {
        if self.unpublished {
            return;
        }
        let mut connection = self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if connection.generation == self.generation {
            connection.outbound.take();
            connection.event_queue_full = false;
            connection.event_producer_waiting = false;
        }
        self.events.mark_not_ready();
        self.unpublished = true;
    }
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.unpublish();
        self.active_slot.store(false, Ordering::Release);
        self.forwarder_abort.abort();
    }
}

pub struct OneBot11Adapter {
    config: OneBot11Config,
    local_addr: SocketAddr,
    listener: Mutex<Option<TcpListener>>,
    connection: Arc<StdMutex<ConnectionState>>,
    media_budget: Arc<Semaphore>,
    media_slots: Arc<Semaphore>,
    #[cfg(test)]
    queue_backpressure: StdMutex<Option<Arc<tokio::sync::Notify>>>,
}

impl std::fmt::Debug for OneBot11Adapter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OneBot11Adapter")
            .field("config", &self.config)
            .field("local_addr", &self.local_addr)
            .finish_non_exhaustive()
    }
}

impl OneBot11Adapter {
    pub async fn bind(config: OneBot11Config) -> Result<Self, AdapterError> {
        if config.access_token.expose_secret().is_empty() {
            return Err(AdapterError::Configuration(
                "OneBot 11 access token must not be empty".to_owned(),
            ));
        }
        if config.max_message_bytes == 0 || config.max_pending_actions == 0 {
            return Err(AdapterError::Configuration(
                "OneBot 11 message and pending Action limits must be greater than zero".to_owned(),
            ));
        }
        if config.action_timeout.is_zero() {
            return Err(AdapterError::Configuration(
                "OneBot 11 action timeout must be greater than zero".to_owned(),
            ));
        }
        if !config.listen.ip().is_loopback() && !config.allow_insecure_remote {
            return Err(AdapterError::Configuration(
                "OneBot 11 non-loopback listener requires allow_insecure_remote because the adapter does not terminate TLS"
                    .to_owned(),
            ));
        }
        let listener = TcpListener::bind(config.listen)
            .await
            .map_err(|error| AdapterError::Transport(error.to_string()))?;
        let local_addr = listener
            .local_addr()
            .map_err(|error| AdapterError::Transport(error.to_string()))?;
        Ok(Self {
            config,
            local_addr,
            listener: Mutex::new(Some(listener)),
            connection: Arc::new(StdMutex::new(ConnectionState::default())),
            media_budget: Arc::new(Semaphore::new(MAX_QUEUED_INLINE_MEDIA_BYTES)),
            media_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_INLINE_MEDIA)),
            #[cfg(test)]
            queue_backpressure: StdMutex::new(None),
        })
    }

    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    #[cfg(test)]
    fn set_queue_backpressure_notify(&self, notify: Arc<tokio::sync::Notify>) {
        *self
            .queue_backpressure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(notify);
    }

    fn start_event_forwarder(
        &self,
        events: EventSender,
    ) -> (
        mpsc::Sender<EventEnvelope>,
        JoinHandle<Result<(), AdapterError>>,
    ) {
        // Reserve one envelope for the forwarder and one for a producer
        // waiting on capacity while preserving the hard connection budget.
        let (queued, receiver) = mpsc::channel(MAX_PENDING_EVENTS_PER_CONNECTION - 3);
        let forwarder = tokio::spawn(forward_events(
            receiver,
            events,
            Arc::clone(&self.connection),
        ));
        (queued, forwarder)
    }

    fn enqueue_local_event(
        &self,
        events: &mpsc::Sender<EventEnvelope>,
        event: EventEnvelope,
        pending_events: &mut VecDeque<EventEnvelope>,
    ) -> Result<TextHandling, AdapterError> {
        {
            let mut connection = self
                .connection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match events.try_send(event) {
                Ok(()) => {
                    connection.event_queue_full = events.capacity() == 0;
                    return Ok(TextHandling::Continue);
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    return Err(AdapterError::EventQueueClosed);
                }
                Err(mpsc::error::TrySendError::Full(event)) => {
                    pending_events.push_back(event);
                    connection.event_queue_full = true;
                    connection.event_producer_waiting = true;
                }
            }
        }
        #[cfg(test)]
        if let Some(notify) = self
            .queue_backpressure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            notify.notify_one();
        }
        Ok(TextHandling::Continue)
    }

    async fn serve_connection(
        &self,
        socket: WebSocketStream<TcpStream>,
        events: &EventSender,
        shutdown: &mut ShutdownSignal,
        active_slot: Arc<AtomicBool>,
    ) -> Result<(), AdapterError> {
        let (outbound_sender, outbound_receiver) = mpsc::channel(self.config.max_pending_actions);
        let generation = {
            let mut connection = self
                .connection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            connection.generation = connection.generation.wrapping_add(1);
            connection.outbound = Some(outbound_sender);
            connection.generation
        };
        let (queued_events, event_forwarder) = self.start_event_forwarder(events.clone());
        let mut connection_guard = ConnectionGuard {
            connection: Arc::clone(&self.connection),
            generation,
            events: events.clone(),
            active_slot,
            forwarder_abort: event_forwarder.abort_handle(),
            unpublished: false,
        };
        events.mark_ready();
        info!(adapter_id = %self.config.id, "OneBot 11 reverse WebSocket connected");
        let result = self
            .run_connection(
                socket,
                outbound_receiver,
                queued_events,
                event_forwarder,
                shutdown,
                &mut connection_guard,
            )
            .await;
        info!(adapter_id = %self.config.id, "OneBot 11 reverse WebSocket disconnected");
        result
    }

    #[allow(clippy::too_many_lines)] // Keep socket, Action, and event teardown in one state machine.
    async fn run_connection(
        &self,
        socket: WebSocketStream<TcpStream>,
        mut outbound: mpsc::Receiver<OutboundAction>,
        queued_events: mpsc::Sender<EventEnvelope>,
        mut event_forwarder: JoinHandle<Result<(), AdapterError>>,
        shutdown: &mut ShutdownSignal,
        connection_guard: &mut ConnectionGuard,
    ) -> Result<(), AdapterError> {
        let (mut writer, mut reader) = socket.split();
        let mut pending = HashMap::<String, PendingAction>::new();
        let mut deadline_tick = interval(Duration::from_millis(100));
        // Keep a hard per-connection memory bound. The reader remains available
        // for Action responses while this buffer has room; once the absolute
        // limit is reached we apply TCP backpressure instead of dropping events
        // or growing memory without bound.
        let mut event_forwarder_finished = false;
        let mut pending_events = VecDeque::new();
        let mut result = loop {
            tokio::select! {
                () = shutdown.cancelled() => {
                    let _ = send_websocket_message(
                        &mut writer,
                        WebSocketMessage::Close(None),
                        Duration::from_secs(1),
                        "OneBot Close",
                    )
                    .await;
                    break Ok(());
                }
                _ = deadline_tick.tick() => {
                    expire_pending(&mut pending);
                }
                forwarded = &mut event_forwarder => {
                    event_forwarder_finished = true;
                    break map_event_forwarder_join(forwarded);
                }
                outbound_action = outbound.recv() => {
                    let Some(outbound_action) = outbound_action else {
                        break Ok(());
                    };
                    if let Err(error) = transmit_action(
                        &mut writer,
                        outbound_action,
                        &mut pending,
                        self.config.max_pending_actions,
                    )
                    .await
                    {
                        break Err(error);
                    }
                }
                reserved = queued_events.reserve(), if !pending_events.is_empty() => {
                    let Ok(permit) = reserved else {
                        break Err(AdapterError::EventQueueClosed);
                    };
                    let event = pending_events
                        .pop_front()
                        .expect("pending event exists while reservation branch is enabled");
                    permit.send(event);
                    let mut connection = self
                        .connection
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    connection.event_queue_full = queued_events.capacity() == 0;
                    connection.event_producer_waiting = !pending_events.is_empty();
                }
                incoming = reader.next(), if pending_events.len() < 2 => {
                    let Some(incoming) = incoming else {
                        break Ok(());
                    };
                    let incoming = match incoming {
                        Ok(incoming) => incoming,
                        Err(error) => {
                            break Err(AdapterError::Transport(error.to_string()));
                        }
                    };
                    match incoming {
                        WebSocketMessage::Text(text) => {
                            match self.handle_text(
                                &text,
                                &queued_events,
                                &mut pending_events,
                                &mut pending,
                            ) {
                                Ok(TextHandling::Continue) => {
                                    if pending_events.len() == 2 {
                                        fail_unsent(&mut outbound);
                                        fail_pending_unknown(&mut pending);
                                    }
                                }
                                Err(error) => break Err(error),
                            }
                        }
                        WebSocketMessage::Binary(_) => {
                            warn!(adapter_id = %self.config.id, "ignored binary OneBot frame");
                        }
                        WebSocketMessage::Ping(payload) => {
                            if let Err(error) = send_websocket_message(
                                &mut writer,
                                WebSocketMessage::Pong(payload),
                                Duration::from_secs(1),
                                "OneBot Pong",
                            )
                            .await
                            {
                                break Err(error);
                            }
                        }
                        WebSocketMessage::Close(_) => break Ok(()),
                        WebSocketMessage::Pong(_) | WebSocketMessage::Frame(_) => {}
                    }
                }
            }
        };
        // Stop accepting Actions as soon as the socket loop ends. Event
        // forwarding may remain backpressured while already accepted events
        // drain, but no Action can be written during that interval.
        connection_guard.unpublish();
        outbound.close();
        fail_unsent(&mut outbound);
        fail_pending_unknown(&mut pending);
        while !pending_events.is_empty() {
            let Ok(permit) = queued_events.reserve().await else {
                if result.is_ok() {
                    result = Err(AdapterError::EventQueueClosed);
                }
                break;
            };
            permit.send(
                pending_events
                    .pop_front()
                    .expect("pending event exists while flushing"),
            );
        }
        drop(queued_events);
        if !event_forwarder_finished {
            if let Err(error) = finish_event_forwarder(&mut event_forwarder).await {
                warn!(adapter_id = %self.config.id, error = %error, "OneBot event forwarder failed while draining");
                if result.is_ok() {
                    result = Err(error);
                }
            }
        }
        result
    }

    fn handle_text(
        &self,
        text: &Utf8Bytes,
        events: &mpsc::Sender<EventEnvelope>,
        pending_events: &mut VecDeque<EventEnvelope>,
        pending: &mut HashMap<String, PendingAction>,
    ) -> Result<TextHandling, AdapterError> {
        let raw: Value = serde_json::from_str(text)
            .map_err(|error| AdapterError::Transport(format!("invalid OneBot JSON: {error}")))?;
        if response_like(&raw) {
            let response: ActionResponse = match serde_json::from_value(raw) {
                Ok(response) => response,
                Err(error) => {
                    warn!(
                        adapter_id = %self.config.id,
                        %error,
                        "ignored malformed OneBot action response"
                    );
                    return Ok(TextHandling::Continue);
                }
            };
            let Some(echo) = response.echo_key() else {
                warn!(adapter_id = %self.config.id, "ignored OneBot response with invalid echo");
                return Ok(TextHandling::Continue);
            };
            if let Some(pending_action) = pending.remove(&echo) {
                let _ = pending_action.response.send(Ok(response));
            } else {
                debug!(adapter_id = %self.config.id, echo, "ignored unknown or duplicate OneBot echo");
            }
            return Ok(TextHandling::Continue);
        }
        match map_event(&self.config.id, raw) {
            Ok(Some(event)) => self.enqueue_local_event(events, event, pending_events),
            Ok(None) => Ok(TextHandling::Continue),
            Err(error) => {
                warn!(adapter_id = %self.config.id, error = %error, "ignored invalid OneBot event");
                Ok(TextHandling::Continue)
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn action_request(action: Action) -> Result<(String, Value), AdapterError> {
        match action {
            Action::Reply(reply) => {
                let mut message = vec![
                    MessageSegment::reply(&OneBotId::new(reply.source_message_id)),
                    MessageSegment::text(reply.content),
                ];
                message
                    .retain(|segment| segment.kind != "text" || segment.text_value() != Some(""));
                match reply.target {
                    MessageTarget::Group { group_id } => Ok((
                        "send_group_msg".to_owned(),
                        json!({"group_id": id_param(&group_id), "message": message}),
                    )),
                    MessageTarget::Private { user_id } => Ok((
                        "send_private_msg".to_owned(),
                        json!({"user_id": id_param(&user_id), "message": message}),
                    )),
                    MessageTarget::Channel { .. } | MessageTarget::GuildDirect { .. } => {
                        Err(AdapterError::Action(
                            "OneBot 11 does not support QQ guild targets".to_owned(),
                        ))
                    }
                }
            }
            Action::SendMessage(send) => match send.target {
                MessageTarget::Group { group_id } => Ok((
                    "send_group_msg".to_owned(),
                    json!({"group_id": id_param(&group_id), "message": [MessageSegment::text(send.content)]}),
                )),
                MessageTarget::Private { user_id } => Ok((
                    "send_private_msg".to_owned(),
                    json!({"user_id": id_param(&user_id), "message": [MessageSegment::text(send.content)]}),
                )),
                MessageTarget::Channel { .. } | MessageTarget::GuildDirect { .. } => Err(
                    AdapterError::Action("OneBot 11 does not support QQ guild targets".to_owned()),
                ),
            },
            Action::ReplyMedia(reply) => {
                let mut message = vec![
                    MessageSegment::reply(&OneBotId::new(reply.source_message_id)),
                    onebot_image_segment(&reply.attachment)?,
                ];
                if let Some(caption) = reply.caption.filter(|value| !value.is_empty()) {
                    message.push(MessageSegment::text(caption));
                }
                match reply.target {
                    MessageTarget::Group { group_id } => Ok((
                        "send_group_msg".to_owned(),
                        json!({"group_id": id_param(&group_id), "message": message}),
                    )),
                    MessageTarget::Private { user_id } => Ok((
                        "send_private_msg".to_owned(),
                        json!({"user_id": id_param(&user_id), "message": message}),
                    )),
                    MessageTarget::Channel { .. } | MessageTarget::GuildDirect { .. } => {
                        Err(AdapterError::Action(
                            "OneBot 11 does not support QQ guild targets".to_owned(),
                        ))
                    }
                }
            }
            Action::SendMedia(send) => {
                let mut message = vec![onebot_image_segment(&send.attachment)?];
                if let Some(caption) = send.caption.filter(|value| !value.is_empty()) {
                    message.push(MessageSegment::text(caption));
                }
                match send.target {
                    MessageTarget::Group { group_id } => Ok((
                        "send_group_msg".to_owned(),
                        json!({"group_id": id_param(&group_id), "message": message}),
                    )),
                    MessageTarget::Private { user_id } => Ok((
                        "send_private_msg".to_owned(),
                        json!({"user_id": id_param(&user_id), "message": message}),
                    )),
                    MessageTarget::Channel { .. } | MessageTarget::GuildDirect { .. } => {
                        Err(AdapterError::Action(
                            "OneBot 11 does not support QQ guild targets".to_owned(),
                        ))
                    }
                }
            }
            Action::Recall { target, message_id } => match target {
                MessageTarget::Group { .. } | MessageTarget::Private { .. } => Ok((
                    "delete_msg".to_owned(),
                    json!({"message_id": id_param(&message_id)}),
                )),
                MessageTarget::Channel { .. } | MessageTarget::GuildDirect { .. } => Err(
                    AdapterError::Action("OneBot 11 does not support QQ guild targets".to_owned()),
                ),
            },
            Action::Platform { name, payload }
                if matches!(
                    name.as_str(),
                    "onebot11.send_msg"
                        | "onebot11.get_login_info"
                        | "onebot11.get_stranger_info"
                        | "onebot11.get_group_info"
                        | "onebot11.get_group_member_info"
                        | "onebot11.get_status"
                        | "onebot11.get_version_info"
                        | "onebot11.send_like"
                        | "onebot11.set_group_kick"
                        | "onebot11.set_group_ban"
                        | "onebot11.set_group_whole_ban"
                        | "onebot11.set_group_admin"
                        | "onebot11.set_group_card"
                        | "onebot11.set_group_leave"
                        | "onebot11.set_friend_add_request"
                        | "onebot11.set_group_add_request"
                ) =>
            {
                let action = name
                    .strip_prefix("onebot11.")
                    .expect("allowlisted OneBot Action has prefix")
                    .to_owned();
                if !payload.is_object() {
                    return Err(AdapterError::Action(format!(
                        "OneBot platform Action `{name}` payload must be a JSON object"
                    )));
                }
                Ok((action, payload))
            }
            Action::Platform { name, .. } => Err(AdapterError::Action(format!(
                "unsupported OneBot 11 platform Action `{name}`"
            ))),
        }
    }
}

#[async_trait]
impl Adapter for OneBot11Adapter {
    fn id(&self) -> &AdapterId {
        &self.config.id
    }

    fn platform(&self) -> &'static str {
        "onebot11"
    }

    async fn run(
        &self,
        events: EventSender,
        mut shutdown: ShutdownSignal,
    ) -> Result<(), AdapterError> {
        let listener = self.listener.lock().await.take().ok_or_else(|| {
            AdapterError::Configuration("OneBot 11 adapter can only be run once".to_owned())
        })?;
        let active_slot = Arc::new(AtomicBool::new(false));
        let mut handshakes = JoinSet::new();
        info!(adapter_id = %self.config.id, listen = %self.local_addr, "OneBot 11 reverse WebSocket listening");
        loop {
            tokio::select! {
                () = shutdown.cancelled() => return Ok(()),
                accepted = listener.accept() => {
                    let (stream, peer) = accepted
                        .map_err(|error| AdapterError::Transport(error.to_string()))?;
                    if handshakes.len() >= MAX_PENDING_HANDSHAKES {
                        reject_busy_connection(stream).await;
                        warn!(adapter_id = %self.config.id, %peer, "rejected excess OneBot 11 handshake");
                        continue;
                    }
                    let access_token = self.config.access_token.clone();
                    let max_message_bytes = self.config.max_message_bytes;
                    let handshake_slot = active_slot.clone();
                    handshakes.spawn(async move {
                        accept_connection(
                            stream,
                            peer,
                            access_token,
                            max_message_bytes,
                            handshake_slot,
                        )
                        .await
                    });
                }
                joined = handshakes.join_next(), if !handshakes.is_empty() => {
                    let Some(joined) = joined else {
                        continue;
                    };
                    let (socket, peer) = match joined {
                        Ok(Ok(connection)) => connection,
                        Ok(Err(error)) => {
                            warn!(adapter_id = %self.config.id, error = %error, "OneBot 11 handshake failed");
                            continue;
                        }
                        Err(error) => {
                            return Err(AdapterError::Transport(format!(
                                "OneBot 11 handshake task failed: {error}"
                            )));
                        }
                    };
                    let connection = self.serve_connection(
                        socket,
                        &events,
                        &mut shutdown,
                        Arc::clone(&active_slot),
                    );
                    tokio::pin!(connection);
                    loop {
                        tokio::select! {
                            result = &mut connection => {
                                if let Err(error) = result {
                                    warn!(adapter_id = %self.config.id, %peer, error = %error, "OneBot 11 connection ended with an error");
                                }
                                break;
                            }
                            accepted = listener.accept() => {
                                let (stream, rejected_peer) = accepted
                                    .map_err(|error| AdapterError::Transport(error.to_string()))?;
                                reject_active_connection(stream).await;
                                warn!(adapter_id = %self.config.id, %rejected_peer, "rejected additional OneBot 11 connection");
                            }
                        }
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn execute(&self, action: Action) -> Result<ActionResult, AdapterError> {
        if matches!(
            &action,
            Action::ReplyMedia(reply)
                if matches!(
                    reply.target,
                    MessageTarget::Channel { .. } | MessageTarget::GuildDirect { .. }
                )
        ) || matches!(
            &action,
            Action::SendMedia(send)
                if matches!(
                    send.target,
                    MessageTarget::Channel { .. } | MessageTarget::GuildDirect { .. }
                )
        ) {
            return Err(AdapterError::Action(
                "OneBot 11 does not support QQ guild targets".to_owned(),
            ));
        }
        let media_bytes = if let Some(attachment) = action_media_attachment(&action) {
            validate_onebot_image_attachment(attachment)?;
            Some(attachment.data().len())
        } else {
            None
        };
        let media_slot = if media_bytes.is_some() {
            Some(
                Arc::clone(&self.media_slots)
                    .try_acquire_owned()
                    .map_err(|_| {
                        AdapterError::Action(
                            "OneBot inline media concurrency limit reached".to_owned(),
                        )
                    })?,
            )
        } else {
            None
        };
        let media_budget = if let Some(media_bytes) = media_bytes {
            let permits = u32::try_from(media_bytes).map_err(|_| {
                AdapterError::Action("OneBot inline image exceeds the media budget".to_owned())
            })?;
            Some(
                Arc::clone(&self.media_budget)
                    .try_acquire_many_owned(permits)
                    .map_err(|_| {
                        AdapterError::Action(
                            "OneBot inline media byte budget is exhausted".to_owned(),
                        )
                    })?,
            )
        } else {
            None
        };
        let ((action, params), media_budget, media_slot) = if media_bytes.is_none() {
            (Self::action_request(action)?, None, None)
        } else {
            let (request, media_budget, media_slot) = tokio::task::spawn_blocking(move || {
                (Self::action_request(action), media_budget, media_slot)
            })
            .await
            .map_err(|error| {
                AdapterError::Action(format!("OneBot inline media encoding task failed: {error}"))
            })?;
            (request?, media_budget, media_slot)
        };
        let (response_sender, response_receiver) = oneshot::channel();
        let deadline = Instant::now() + self.config.action_timeout;
        {
            // Admission and connection retirement share this lock, so a
            // sender snapshot cannot enqueue after the socket is unpublished.
            let connection = self
                .connection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let sender = connection.outbound.as_ref().ok_or_else(|| {
                AdapterError::Action("OneBot 11 reverse WebSocket is not connected".to_owned())
            })?;
            if connection.event_queue_full || connection.event_producer_waiting {
                return Err(AdapterError::Action(
                    "OneBot event queue is backpressured; Action was not sent".to_owned(),
                ));
            }
            sender
                .try_send(OutboundAction {
                    action,
                    params,
                    deadline,
                    response: response_sender,
                    media_budget,
                    media_slot,
                })
                .map_err(|error| {
                    AdapterError::Action(format!("OneBot Action queue unavailable: {error}"))
                })?;
        }
        let response = timeout(
            self.config
                .action_timeout
                .saturating_add(Duration::from_secs(1)),
            response_receiver,
        )
        .await
        .map_err(|_| AdapterError::ActionUnknown("OneBot Action result timed out".to_owned()))?
        .map_err(|_| {
            AdapterError::ActionUnknown("OneBot Action result channel closed".to_owned())
        })??;
        if !response.succeeded() {
            return Err(AdapterError::Action(response.safe_error()));
        }
        Ok(ActionResult {
            message_id: response.message_id(),
            raw: serde_json::to_value(response)
                .map_err(|error| AdapterError::Action(error.to_string()))?,
        })
    }
}

async fn forward_events(
    mut queued: mpsc::Receiver<EventEnvelope>,
    events: EventSender,
    connection: Arc<StdMutex<ConnectionState>>,
) -> Result<(), AdapterError> {
    while let Some(event) = queued.recv().await {
        {
            let mut connection_state = connection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            connection_state.event_queue_full = queued.capacity() == 0;
        }
        let permit = match events.reserve().await {
            Ok(permit) => permit,
            Err(EventSendError::QueueClosed) => return Err(AdapterError::EventQueueClosed),
            Err(EventSendError::QueueFull) => {
                return Err(AdapterError::Transport(
                    "runtime event queue unexpectedly remained full after waiting".to_owned(),
                ));
            }
            Err(EventSendError::AdapterMismatch { expected, actual }) => {
                return Err(AdapterError::EventAdapterMismatch { expected, actual });
            }
        };
        match permit.send(event) {
            Ok(()) => {}
            Err(EventSendError::QueueClosed) => return Err(AdapterError::EventQueueClosed),
            Err(EventSendError::QueueFull) => {
                return Err(AdapterError::Transport(
                    "runtime event queue unexpectedly remained full after waiting".to_owned(),
                ));
            }
            Err(EventSendError::AdapterMismatch { expected, actual }) => {
                return Err(AdapterError::EventAdapterMismatch { expected, actual });
            }
        }
    }
    Ok(())
}

async fn finish_event_forwarder(
    forwarder: &mut JoinHandle<Result<(), AdapterError>>,
) -> Result<(), AdapterError> {
    map_event_forwarder_join(forwarder.await)
}

fn map_event_forwarder_join(
    joined: Result<Result<(), AdapterError>, tokio::task::JoinError>,
) -> Result<(), AdapterError> {
    joined.unwrap_or_else(|error| {
        Err(AdapterError::Transport(format!(
            "OneBot event forwarding task failed: {error}"
        )))
    })
}

fn id_param(value: &str) -> Value {
    Value::String(value.to_owned())
}

fn onebot_image_segment(attachment: &MediaAttachment) -> Result<MessageSegment, AdapterError> {
    validate_onebot_image_attachment(attachment)?;
    Ok(MessageSegment::image_bytes(attachment.data()))
}

fn validate_onebot_image_attachment(attachment: &MediaAttachment) -> Result<(), AdapterError> {
    if attachment.validated_image_mime().is_none() {
        return Err(AdapterError::Action(format!(
            "OneBot 11 inline media has an unsupported MIME type or mismatched image signature: `{}`",
            attachment.mime_type()
        )));
    }
    if attachment.data().is_empty() {
        return Err(AdapterError::Action(
            "OneBot 11 inline image data must not be empty".to_owned(),
        ));
    }
    if attachment.data().len() > MAX_INLINE_MEDIA_BYTES {
        return Err(AdapterError::Action(
            "OneBot 11 inline image exceeds the 8 MiB limit".to_owned(),
        ));
    }
    Ok(())
}

fn action_media_attachment(action: &Action) -> Option<&MediaAttachment> {
    match action {
        Action::ReplyMedia(reply) => Some(&reply.attachment),
        Action::SendMedia(send) => Some(&send.attachment),
        _ => None,
    }
}

async fn send_websocket_message<S>(
    writer: &mut S,
    message: WebSocketMessage,
    write_timeout: Duration,
    operation: &str,
) -> Result<(), AdapterError>
where
    S: Sink<WebSocketMessage, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    match timeout(write_timeout, writer.send(message)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(AdapterError::Transport(error.to_string())),
        Err(_) => Err(AdapterError::Transport(format!(
            "{operation} write timed out"
        ))),
    }
}

async fn transmit_action<S>(
    writer: &mut S,
    outbound: OutboundAction,
    pending: &mut HashMap<String, PendingAction>,
    max_pending_actions: usize,
) -> Result<(), AdapterError>
where
    S: Sink<WebSocketMessage, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    if outbound.deadline <= Instant::now() {
        let _ = outbound.response.send(Err(AdapterError::Action(
            "OneBot action expired before it was sent".to_owned(),
        )));
        return Ok(());
    }
    if pending.len() >= max_pending_actions {
        let _ = outbound.response.send(Err(AdapterError::Action(
            "OneBot pending Action limit reached".to_owned(),
        )));
        return Ok(());
    }
    let echo = Uuid::new_v4().to_string();
    let request = ActionRequest {
        action: outbound.action,
        params: outbound.params,
        echo: echo.clone(),
    };
    let encoded = match serde_json::to_string(&request) {
        Ok(encoded) => encoded,
        Err(error) => {
            let _ = outbound
                .response
                .send(Err(AdapterError::Action(error.to_string())));
            return Ok(());
        }
    };
    pending.insert(
        echo.clone(),
        PendingAction {
            deadline: outbound.deadline,
            response: outbound.response,
            _media_budget: outbound.media_budget,
            _media_slot: outbound.media_slot,
        },
    );
    let write_timeout = outbound.deadline.saturating_duration_since(Instant::now());
    if let Err(error) = send_websocket_message(
        writer,
        WebSocketMessage::Text(encoded.into()),
        write_timeout,
        "OneBot Action",
    )
    .await
    {
        if let Some(pending_action) = pending.remove(&echo) {
            let _ = pending_action
                .response
                .send(Err(AdapterError::ActionUnknown(
                    "OneBot connection closed after Action transmission started".to_owned(),
                )));
        }
        return Err(error);
    }
    Ok(())
}

#[allow(clippy::result_large_err)]
async fn accept_connection(
    stream: TcpStream,
    peer: SocketAddr,
    access_token: SecretString,
    max_message_bytes: usize,
    active_slot: Arc<AtomicBool>,
) -> Result<(WebSocketStream<TcpStream>, SocketAddr), AdapterError> {
    let mut config = WebSocketConfig::default();
    config.max_message_size = Some(max_message_bytes);
    config.max_frame_size = Some(max_message_bytes);
    let claimed = Arc::new(AtomicBool::new(false));
    let callback_claimed = claimed.clone();
    let callback_slot = active_slot.clone();
    let handshake = timeout(
        Duration::from_secs(10),
        accept_hdr_async_with_config(
            stream,
            move |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                  response| {
                if !request_authorized(request, access_token.expose_secret()) {
                    return Err(unauthorized_response());
                }
                if callback_slot
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    return Err(active_connection_response());
                }
                callback_claimed.store(true, Ordering::Release);
                Ok(response)
            },
            Some(config),
        ),
    )
    .await;
    match handshake {
        Ok(Ok(socket)) => Ok((socket, peer)),
        Ok(Err(error)) => {
            if claimed.load(Ordering::Acquire) {
                active_slot.store(false, Ordering::Release);
            }
            Err(AdapterError::Transport(error.to_string()))
        }
        Err(_) => {
            if claimed.load(Ordering::Acquire) {
                active_slot.store(false, Ordering::Release);
            }
            Err(AdapterError::Transport(
                "OneBot WebSocket handshake timed out".to_owned(),
            ))
        }
    }
}

fn request_authorized(
    request: &tokio_tungstenite::tungstenite::handshake::server::Request,
    expected: &str,
) -> bool {
    let authorization = request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let query_token = request.uri().query().and_then(|query| {
        url::form_urlencoded::parse(query.as_bytes())
            .find(|(key, _)| key == "access_token")
            .map(|(_, value)| value.into_owned())
    });
    authorization.is_some_and(|value| constant_time_eq(value.as_bytes(), expected.as_bytes()))
        || query_token
            .as_deref()
            .is_some_and(|value| constant_time_eq(value.as_bytes(), expected.as_bytes()))
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    Sha256::digest(left).ct_eq(&Sha256::digest(right)).into()
}

fn unauthorized_response() -> ErrorResponse {
    let mut response = ErrorResponse::new(Some("unauthorized".to_owned()));
    *response.status_mut() = StatusCode::UNAUTHORIZED;
    response
}

fn active_connection_response() -> ErrorResponse {
    let mut response = ErrorResponse::new(Some("connection already active".to_owned()));
    *response.status_mut() = StatusCode::CONFLICT;
    response
}

async fn reject_active_connection(mut stream: TcpStream) {
    const RESPONSE: &[u8] = b"HTTP/1.1 409 Conflict\r\nConnection: close\r\nContent-Length: 36\r\nContent-Type: text/plain\r\n\r\nOneBot connection is already active\n";
    let _ = timeout(Duration::from_secs(1), stream.write_all(RESPONSE)).await;
    let _ = stream.shutdown().await;
}

async fn reject_busy_connection(mut stream: TcpStream) {
    const RESPONSE: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\nContent-Length: 35\r\nContent-Type: text/plain\r\n\r\nToo many pending OneBot handshakes\n";
    let _ = timeout(Duration::from_secs(1), stream.write_all(RESPONSE)).await;
    let _ = stream.shutdown().await;
}

fn expire_pending(pending: &mut HashMap<String, PendingAction>) {
    let now = Instant::now();
    let expired = pending
        .iter()
        .filter(|(_, pending)| pending.deadline <= now)
        .map(|(echo, _)| echo.clone())
        .collect::<Vec<_>>();
    for echo in expired {
        if let Some(pending) = pending.remove(&echo) {
            let _ = pending.response.send(Err(AdapterError::ActionUnknown(
                "OneBot Action result timed out".to_owned(),
            )));
        }
    }
}

fn fail_pending_unknown(pending: &mut HashMap<String, PendingAction>) {
    for (_, pending) in pending.drain() {
        let _ = pending.response.send(Err(AdapterError::ActionUnknown(
            "OneBot connection closed before Action result".to_owned(),
        )));
    }
}

fn fail_unsent(outbound: &mut mpsc::Receiver<OutboundAction>) {
    while let Ok(action) = outbound.try_recv() {
        let _ = action.response.send(Err(AdapterError::Action(
            "OneBot connection closed before Action was sent".to_owned(),
        )));
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::Arc, time::Duration};

    use bot_core::{
        Action, Adapter, AdapterError, AdapterId, Event, EventEnvelope, EventSender,
        MediaAttachment, MessageTarget, ReplyAction, ReplyMediaAction, RuntimeBuilder,
        RuntimeObserver, SendMediaAction, SendMessageAction, shutdown_channel,
    };
    use builtin_plugins::PingPlugin;
    use futures_util::{SinkExt as _, StreamExt as _};
    use plugin_host::{PluginStore, StaticPluginHost};
    use secrecy::SecretString;
    use serde_json::{Value, json};
    use tokio::time::timeout;
    use tokio_tungstenite::{
        connect_async,
        tungstenite::{Message as WebSocketMessage, client::IntoClientRequest as _},
    };

    use super::{MAX_INLINE_MEDIA_BYTES, OneBot11Adapter, OneBot11Config, onebot_image_segment};

    async fn adapter() -> Arc<OneBot11Adapter> {
        adapter_with_timeout(Duration::from_secs(2)).await
    }

    async fn adapter_with_timeout(action_timeout: Duration) -> Arc<OneBot11Adapter> {
        Arc::new(
            OneBot11Adapter::bind(OneBot11Config {
                id: AdapterId::new("onebot-test"),
                listen: "127.0.0.1:0".parse().unwrap(),
                access_token: SecretString::from("test-token".to_owned()),
                allow_insecure_remote: false,
                action_timeout,
                max_message_bytes: 64 * 1024,
                max_pending_actions: 8,
            })
            .await
            .unwrap(),
        )
    }

    fn event_channel(capacity: usize) -> (EventSender, tokio::sync::mpsc::Receiver<EventEnvelope>) {
        let (sender, receiver) = tokio::sync::mpsc::channel(capacity);
        (
            EventSender::new(
                sender,
                AdapterId::new("onebot-test"),
                RuntimeObserver::new(),
            )
            .unwrap(),
            receiver,
        )
    }

    fn request(
        adapter: &OneBot11Adapter,
        token: &str,
    ) -> tokio_tungstenite::tungstenite::http::Request<()> {
        let mut request = format!("ws://{}/", adapter.local_addr())
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
        request
    }

    fn query_request(
        adapter: &OneBot11Adapter,
        token: &str,
    ) -> tokio_tungstenite::tungstenite::http::Request<()> {
        format!(
            "ws://{}/?access_token={}",
            adapter.local_addr(),
            url::form_urlencoded::byte_serialize(token.as_bytes()).collect::<String>()
        )
        .into_client_request()
        .unwrap()
    }

    #[tokio::test]
    async fn reverse_websocket_rejects_invalid_token() {
        let adapter = adapter().await;
        let (shutdown_handle, shutdown_signal) = shutdown_channel();
        let (events, _received) = event_channel(1);
        let running_adapter = adapter.clone();
        let task = tokio::spawn(async move { running_adapter.run(events, shutdown_signal).await });

        let error = connect_async(request(&adapter, "wrong-token"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("401"));
        shutdown_handle.shutdown();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn non_loopback_listener_requires_explicit_opt_in() {
        let error = OneBot11Adapter::bind(OneBot11Config {
            listen: "0.0.0.0:0".parse().unwrap(),
            access_token: SecretString::from("test-token".to_owned()),
            ..OneBot11Config::default()
        })
        .await
        .unwrap_err();
        assert!(matches!(error, bot_core::AdapterError::Configuration(_)));
    }

    #[tokio::test]
    async fn reverse_websocket_accepts_query_token() {
        let adapter = adapter().await;
        let (shutdown_handle, shutdown_signal) = shutdown_channel();
        let (events, _received) = event_channel(1);
        let running_adapter = adapter.clone();
        let task = tokio::spawn(async move { running_adapter.run(events, shutdown_signal).await });

        let (mut socket, _) = connect_async(query_request(&adapter, "test-token"))
            .await
            .unwrap();
        socket.close(None).await.unwrap();
        shutdown_handle.shutdown();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn stalled_handshake_does_not_block_authorized_client() {
        let adapter = adapter().await;
        let (shutdown_handle, shutdown_signal) = shutdown_channel();
        let (events, _received) = event_channel(1);
        let running_adapter = adapter.clone();
        let task = tokio::spawn(async move { running_adapter.run(events, shutdown_signal).await });
        let _stalled = tokio::net::TcpStream::connect(adapter.local_addr())
            .await
            .unwrap();

        let (mut socket, _) = timeout(
            Duration::from_secs(1),
            connect_async(request(&adapter, "test-token")),
        )
        .await
        .unwrap()
        .unwrap();
        socket.close(None).await.unwrap();

        shutdown_handle.shutdown();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn same_ping_plugin_replies_through_onebot_reverse_websocket() {
        let adapter = adapter().await;
        let mut plugins =
            StaticPluginHost::new(PluginStore::in_memory().unwrap()).with_adapter(adapter.clone());
        plugins
            .register_trusted(
                Arc::new(PingPlugin::default()),
                "dev.bkm.ping/onebot-test",
                BTreeMap::new(),
            )
            .await
            .unwrap();
        let (shutdown_handle, shutdown_signal) = shutdown_channel();
        let runtime = RuntimeBuilder::new()
            .adapter(adapter.clone())
            .handler(Arc::new(plugins))
            .build()
            .unwrap();
        let runtime_task = tokio::spawn(runtime.run(shutdown_signal));
        let (mut socket, _) = connect_async(request(&adapter, "test-token"))
            .await
            .unwrap();
        socket
            .send(WebSocketMessage::Text(
                json!({
                    "time": 1_700_000_000,
                    "self_id": 10000,
                    "post_type": "message",
                    "message_type": "group",
                    "sub_type": "normal",
                    "message_id": 200,
                    "group_id": 300,
                    "user_id": 400,
                    "message": [{"type":"text","data":{"text":"/ping"}}],
                    "raw_message": "/ping",
                    "sender": {"nickname":"Tester"}
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

        let request = timeout(Duration::from_secs(2), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let WebSocketMessage::Text(request) = request else {
            panic!("expected OneBot Action request");
        };
        let request: Value = serde_json::from_str(&request).unwrap();
        assert_eq!(request["action"], "send_group_msg");
        assert_eq!(request["params"]["group_id"], "300");
        assert_eq!(request["params"]["message"][1]["data"]["text"], "pong");
        let echo = request["echo"].clone();
        socket
            .send(WebSocketMessage::Text(
                json!({
                    "status":"ok",
                    "retcode":0,
                    "data":{"message_id":201},
                    "echo":echo
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

        shutdown_handle.shutdown();
        runtime_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn disconnect_marks_transmitted_action_result_unknown() {
        let adapter = adapter().await;
        let (shutdown_handle, shutdown_signal) = shutdown_channel();
        let (events, _received) = event_channel(1);
        let running_adapter = adapter.clone();
        let task = tokio::spawn(async move { running_adapter.run(events, shutdown_signal).await });
        let (mut socket, _) = connect_async(request(&adapter, "test-token"))
            .await
            .unwrap();

        let executing = {
            let adapter = adapter.clone();
            tokio::spawn(async move {
                adapter
                    .execute(Action::Reply(ReplyAction {
                        target: MessageTarget::Private {
                            user_id: "42".to_owned(),
                        },
                        source_message_id: "7".to_owned(),
                        content: "pong".to_owned(),
                    }))
                    .await
            })
        };
        let request = timeout(Duration::from_secs(2), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(request, WebSocketMessage::Text(_)));
        socket.close(None).await.unwrap();
        let error = executing.await.unwrap().unwrap_err();
        assert!(matches!(error, bot_core::AdapterError::ActionUnknown(_)));

        shutdown_handle.shutdown();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn aborting_adapter_task_unpublishes_connection_and_fails_actions() {
        let adapter = adapter().await;
        let (_shutdown_handle, shutdown_signal) = shutdown_channel();
        let (events, _received) = event_channel(1);
        let running_adapter = adapter.clone();
        let task = tokio::spawn(async move { running_adapter.run(events, shutdown_signal).await });
        let (mut socket, _) = connect_async(request(&adapter, "test-token"))
            .await
            .unwrap();

        let executing = {
            let adapter = adapter.clone();
            tokio::spawn(async move {
                adapter
                    .execute(Action::SendMessage(SendMessageAction {
                        target: MessageTarget::Private {
                            user_id: "42".to_owned(),
                        },
                        content: "cancelled".to_owned(),
                    }))
                    .await
            })
        };
        assert!(matches!(
            socket.next().await.unwrap().unwrap(),
            WebSocketMessage::Text(_)
        ));
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(
            adapter
                .connection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .outbound
                .is_none()
        );
        assert!(matches!(
            timeout(Duration::from_secs(1), executing)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err(),
            AdapterError::ActionUnknown(_)
        ));
    }

    #[tokio::test]
    async fn action_timeout_is_reported_as_unknown() {
        let adapter = adapter_with_timeout(Duration::from_millis(100)).await;
        let (shutdown_handle, shutdown_signal) = shutdown_channel();
        let (events, _received) = event_channel(1);
        let running_adapter = adapter.clone();
        let task = tokio::spawn(async move { running_adapter.run(events, shutdown_signal).await });
        let (mut socket, _) = connect_async(request(&adapter, "test-token"))
            .await
            .unwrap();

        let executing = {
            let adapter = adapter.clone();
            tokio::spawn(async move {
                adapter
                    .execute(Action::SendMessage(SendMessageAction {
                        target: MessageTarget::Group {
                            group_id: "300".to_owned(),
                        },
                        content: "hello".to_owned(),
                    }))
                    .await
            })
        };
        timeout(Duration::from_secs(1), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let error = timeout(Duration::from_secs(1), executing)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(matches!(error, bot_core::AdapterError::ActionUnknown(_)));

        shutdown_handle.shutdown();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn unknown_and_duplicate_echoes_do_not_break_correlation() {
        let adapter = adapter().await;
        let (shutdown_handle, shutdown_signal) = shutdown_channel();
        let (events, _received) = event_channel(1);
        let running_adapter = adapter.clone();
        let task = tokio::spawn(async move { running_adapter.run(events, shutdown_signal).await });
        let (mut socket, _) = connect_async(request(&adapter, "test-token"))
            .await
            .unwrap();

        for index in 0..2 {
            let executing = {
                let adapter = adapter.clone();
                tokio::spawn(async move {
                    adapter
                        .execute(Action::SendMessage(SendMessageAction {
                            target: MessageTarget::Private {
                                user_id: "42".to_owned(),
                            },
                            content: format!("message-{index}"),
                        }))
                        .await
                })
            };
            let WebSocketMessage::Text(request) = socket.next().await.unwrap().unwrap() else {
                panic!("expected action request");
            };
            let request: Value = serde_json::from_str(&request).unwrap();
            if index == 0 {
                socket
                    .send(WebSocketMessage::Text(
                        json!({"status":"ok","retcode":0,"data":{},"echo":"unknown"})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
            }
            let response = json!({
                "status":"ok",
                "retcode":0,
                "data":{"message_id":index},
                "echo":request["echo"]
            });
            socket
                .send(WebSocketMessage::Text(response.to_string().into()))
                .await
                .unwrap();
            executing.await.unwrap().unwrap();
            if index == 0 {
                socket
                    .send(WebSocketMessage::Text(response.to_string().into()))
                    .await
                    .unwrap();
            }
        }

        shutdown_handle.shutdown();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn saturated_event_queue_does_not_block_action_response() {
        let adapter = adapter().await;
        let (shutdown_handle, shutdown_signal) = shutdown_channel();
        let (events, received) = event_channel(1);
        events
            .try_send(
                super::map_event(
                    &AdapterId::new("onebot-test"),
                    serde_json::from_str(include_str!(
                        "../../../test-data/onebot11/group-message.json"
                    ))
                    .unwrap(),
                )
                .unwrap()
                .unwrap(),
            )
            .unwrap();
        let running_adapter = adapter.clone();
        let task = tokio::spawn(async move { running_adapter.run(events, shutdown_signal).await });
        let (mut socket, _) = connect_async(request(&adapter, "test-token"))
            .await
            .unwrap();
        socket
            .send(WebSocketMessage::Text(
                include_str!("../../../test-data/onebot11/private-message.json").into(),
            ))
            .await
            .unwrap();

        let executing = {
            let adapter = adapter.clone();
            tokio::spawn(async move {
                adapter
                    .execute(Action::SendMessage(SendMessageAction {
                        target: MessageTarget::Private {
                            user_id: "42".to_owned(),
                        },
                        content: "hello".to_owned(),
                    }))
                    .await
            })
        };
        let WebSocketMessage::Text(request) = socket.next().await.unwrap().unwrap() else {
            panic!("expected action request");
        };
        let request: Value = serde_json::from_str(&request).unwrap();
        socket
            .send(WebSocketMessage::Text(
                json!({"status":"ok","retcode":0,"data":{},"echo":request["echo"]})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        timeout(Duration::from_secs(1), executing)
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        shutdown_handle.shutdown();
        let drain = tokio::spawn(async move {
            let mut received = received;
            while received.recv().await.is_some() {}
        });
        timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        drain.await.unwrap();
    }

    #[tokio::test]
    async fn filling_the_final_local_event_slot_rejects_new_actions() {
        let adapter = adapter().await;
        let (shutdown_handle, shutdown_signal) = shutdown_channel();
        let (events, received) = event_channel(1);
        events
            .try_send(
                super::map_event(
                    &AdapterId::new("onebot-test"),
                    serde_json::from_str(include_str!(
                        "../../../test-data/onebot11/group-message.json"
                    ))
                    .unwrap(),
                )
                .unwrap()
                .unwrap(),
            )
            .unwrap();
        let running_adapter = adapter.clone();
        let task = tokio::spawn(async move { running_adapter.run(events, shutdown_signal).await });
        let (mut socket, _) = connect_async(request(&adapter, "test-token"))
            .await
            .unwrap();
        for _ in 0..super::MAX_PENDING_EVENTS_PER_CONNECTION - 1 {
            socket
                .send(WebSocketMessage::Text(
                    include_str!("../../../test-data/onebot11/private-message.json").into(),
                ))
                .await
                .unwrap();
        }
        timeout(Duration::from_secs(1), async {
            while !adapter
                .connection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .event_queue_full
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let error = adapter
            .execute(Action::SendMessage(SendMessageAction {
                target: MessageTarget::Private {
                    user_id: "42".to_owned(),
                },
                content: "must-not-send".to_owned(),
            }))
            .await
            .unwrap_err();
        assert!(
            matches!(error, AdapterError::Action(message) if message.contains("backpressured"))
        );

        shutdown_handle.shutdown();
        let drain = tokio::spawn(async move {
            let mut received = received;
            while received.recv().await.is_some() {}
        });
        timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        drain.await.unwrap();
    }

    #[tokio::test]
    async fn disconnected_socket_is_unpublished_before_accepted_events_finish_draining() {
        let adapter = adapter().await;
        let (shutdown_handle, shutdown_signal) = shutdown_channel();
        let (events, mut received) = event_channel(1);
        events
            .try_send(
                super::map_event(
                    &AdapterId::new("onebot-test"),
                    serde_json::from_str(include_str!(
                        "../../../test-data/onebot11/group-message.json"
                    ))
                    .unwrap(),
                )
                .unwrap()
                .unwrap(),
            )
            .unwrap();
        let running_adapter = adapter.clone();
        let task = tokio::spawn(async move { running_adapter.run(events, shutdown_signal).await });
        let (mut socket, _) = connect_async(request(&adapter, "test-token"))
            .await
            .unwrap();
        socket
            .send(WebSocketMessage::Text(
                include_str!("../../../test-data/onebot11/private-message.json").into(),
            ))
            .await
            .unwrap();
        socket.close(None).await.unwrap();

        timeout(Duration::from_secs(1), async {
            loop {
                if adapter
                    .connection
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .outbound
                    .is_none()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let error = adapter
            .execute(Action::SendMessage(SendMessageAction {
                target: MessageTarget::Private {
                    user_id: "42".to_owned(),
                },
                content: "must-not-queue".to_owned(),
            }))
            .await
            .unwrap_err();
        assert!(
            matches!(error, AdapterError::Action(message) if message.contains("not connected"))
        );

        received.recv().await.unwrap();
        received.recv().await.unwrap();
        shutdown_handle.shutdown();
        timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn shutdown_cancels_a_connection_blocked_on_its_local_event_queue() {
        let adapter = adapter().await;
        let saturated = Arc::new(tokio::sync::Notify::new());
        adapter.set_queue_backpressure_notify(Arc::clone(&saturated));
        let (shutdown_handle, shutdown_signal) = shutdown_channel();
        let (events, received) = event_channel(1);
        events
            .try_send(
                super::map_event(
                    &AdapterId::new("onebot-test"),
                    serde_json::from_str(include_str!(
                        "../../../test-data/onebot11/group-message.json"
                    ))
                    .unwrap(),
                )
                .unwrap()
                .unwrap(),
            )
            .unwrap();
        let running_adapter = adapter.clone();
        let task = tokio::spawn(async move { running_adapter.run(events, shutdown_signal).await });
        let (mut socket, _) = connect_async(request(&adapter, "test-token"))
            .await
            .unwrap();
        for _ in 0..super::MAX_PENDING_EVENTS_PER_CONNECTION + 2 {
            socket
                .send(WebSocketMessage::Text(
                    include_str!("../../../test-data/onebot11/private-message.json").into(),
                ))
                .await
                .unwrap();
        }
        timeout(Duration::from_secs(1), saturated.notified())
            .await
            .unwrap();

        let error = adapter
            .execute(Action::SendMessage(SendMessageAction {
                target: MessageTarget::Private {
                    user_id: "42".to_owned(),
                },
                content: "must-not-send".to_owned(),
            }))
            .await
            .unwrap_err();
        assert!(
            matches!(error, AdapterError::Action(message) if message.contains("backpressured"))
        );

        shutdown_handle.shutdown();
        let drain = tokio::spawn(async move {
            let mut received = received;
            while received.recv().await.is_some() {}
        });
        timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        drain.await.unwrap();
    }

    #[tokio::test]
    async fn malformed_response_like_json_does_not_close_connection() {
        let adapter = adapter().await;
        let (shutdown_handle, shutdown_signal) = shutdown_channel();
        let (events, _received) = event_channel(1);
        let running_adapter = adapter.clone();
        let task = tokio::spawn(async move { running_adapter.run(events, shutdown_signal).await });
        let (mut socket, _) = connect_async(request(&adapter, "test-token"))
            .await
            .unwrap();
        socket
            .send(WebSocketMessage::Text(
                json!({"status":"ok","retcode":"0","echo":"malformed"})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;

        let executing = {
            let adapter = adapter.clone();
            tokio::spawn(async move {
                adapter
                    .execute(Action::SendMessage(SendMessageAction {
                        target: MessageTarget::Private {
                            user_id: "42".to_owned(),
                        },
                        content: "still connected".to_owned(),
                    }))
                    .await
            })
        };
        let WebSocketMessage::Text(request) = socket.next().await.unwrap().unwrap() else {
            panic!("expected action request");
        };
        let request: Value = serde_json::from_str(&request).unwrap();
        socket
            .send(WebSocketMessage::Text(
                json!({"status":"ok","retcode":0,"data":{},"echo":request["echo"]})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        executing.await.unwrap().unwrap();

        shutdown_handle.shutdown();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn additional_connection_is_rejected_and_reconnect_succeeds() {
        let adapter = adapter().await;
        let (shutdown_handle, shutdown_signal) = shutdown_channel();
        let (events, _received) = event_channel(1);
        let running_adapter = adapter.clone();
        let task = tokio::spawn(async move { running_adapter.run(events, shutdown_signal).await });
        let (mut first, _) = connect_async(request(&adapter, "test-token"))
            .await
            .unwrap();

        let error = connect_async(request(&adapter, "test-token"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("409"));
        first.close(None).await.unwrap();

        let reconnect = timeout(Duration::from_secs(2), async {
            loop {
                match connect_async(request(&adapter, "test-token")).await {
                    Ok(connection) => break connection,
                    Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
                }
            }
        })
        .await
        .unwrap();
        let (mut reconnected, _) = reconnect;
        reconnected.close(None).await.unwrap();

        shutdown_handle.shutdown();
        task.await.unwrap().unwrap();
    }

    #[test]
    fn explicit_send_msg_platform_action_is_supported() {
        let (action, params) = OneBot11Adapter::action_request(Action::Platform {
            name: "onebot11.send_msg".to_owned(),
            payload: json!({"message_type":"private","user_id":"42","message":"hello"}),
        })
        .unwrap();
        assert_eq!(action, "send_msg");
        assert_eq!(params["user_id"], "42");

        let (action, params) = OneBot11Adapter::action_request(Action::Platform {
            name: "onebot11.set_group_ban".to_owned(),
            payload: json!({"group_id":"300","user_id":"42","duration":60}),
        })
        .unwrap();
        assert_eq!(action, "set_group_ban");
        assert_eq!(params["duration"], 60);

        assert!(
            OneBot11Adapter::action_request(Action::Platform {
                name: "onebot11.set_group_ban".to_owned(),
                payload: json!("invalid"),
            })
            .is_err()
        );
        assert!(
            OneBot11Adapter::action_request(Action::Platform {
                name: "onebot11.set_group_anonymous_ban".to_owned(),
                payload: json!({}),
            })
            .is_err()
        );
    }

    #[test]
    fn common_private_send_and_recall_actions_are_supported() {
        let (action, params) =
            OneBot11Adapter::action_request(Action::SendMessage(SendMessageAction {
                target: MessageTarget::Private {
                    user_id: "0042".to_owned(),
                },
                content: "hello".to_owned(),
            }))
            .unwrap();
        assert_eq!(action, "send_private_msg");
        assert_eq!(params["user_id"], "0042");

        let (action, params) = OneBot11Adapter::action_request(Action::Recall {
            target: MessageTarget::Group {
                group_id: "300".to_owned(),
            },
            message_id: "0007".to_owned(),
        })
        .unwrap();
        assert_eq!(action, "delete_msg");
        assert_eq!(params["message_id"], "0007");

        let error = OneBot11Adapter::action_request(Action::Recall {
            target: MessageTarget::GuildDirect {
                guild_id: "direct-guild".to_owned(),
            },
            message_id: "0008".to_owned(),
        })
        .unwrap_err();
        assert!(matches!(error, AdapterError::Action(message) if message.contains("guild")));
    }

    #[test]
    fn common_media_requires_a_non_empty_image_attachment() {
        let image = Action::SendMedia(SendMediaAction {
            target: MessageTarget::Private {
                user_id: "0042".to_owned(),
            },
            attachment: MediaAttachment::image("image/png", None, b"\x89PNG\r\n\x1a\n".to_vec())
                .unwrap(),
            caption: None,
        });
        let (action, params) = OneBot11Adapter::action_request(image).unwrap();
        assert_eq!(action, "send_private_msg");
        assert_eq!(params["message"][0]["type"], "image");
        assert_eq!(
            params["message"][0]["data"]["file"],
            "base64://iVBORw0KGgo="
        );

        let (action, params) =
            OneBot11Adapter::action_request(Action::ReplyMedia(ReplyMediaAction {
                target: MessageTarget::Group {
                    group_id: "0300".to_owned(),
                },
                source_message_id: "0007".to_owned(),
                attachment: MediaAttachment::image(
                    "image/png",
                    None,
                    b"\x89PNG\r\n\x1a\n".to_vec(),
                )
                .unwrap(),
                caption: Some("caption".to_owned()),
            }))
            .unwrap();
        assert_eq!(action, "send_group_msg");
        assert_eq!(params["group_id"], "0300");
        assert_eq!(params["message"][0]["type"], "reply");
        assert_eq!(params["message"][0]["data"]["id"], "0007");
        assert_eq!(params["message"][1]["type"], "image");
        assert_eq!(params["message"][2]["type"], "text");
        assert_eq!(params["message"][2]["data"]["text"], "caption");

        assert!(
            OneBot11Adapter::action_request(Action::SendMedia(SendMediaAction {
                target: MessageTarget::Channel {
                    channel_id: "channel".to_owned(),
                },
                attachment: MediaAttachment::image(
                    "image/png",
                    None,
                    b"\x89PNG\r\n\x1a\n".to_vec(),
                )
                .unwrap(),
                caption: None,
            }))
            .is_err()
        );

        assert!(MediaAttachment::image("video/mp4", None, vec![1]).is_err());
        assert!(MediaAttachment::image("image/png", None, Vec::new()).is_err());
        assert!(MediaAttachment::image("image/png", None, b"not an image".to_vec()).is_err());
        let mut oversized = b"\x89PNG\r\n\x1a\n".to_vec();
        oversized.resize(MAX_INLINE_MEDIA_BYTES + 1, 0);
        assert!(MediaAttachment::image("image/png", None, oversized).is_err());
    }

    #[test]
    fn common_media_accepts_each_supported_image_signature_and_the_size_boundary() {
        for (mime_type, data) in [
            ("image/png", b"\x89PNG\r\n\x1a\n".as_slice()),
            ("image/jpeg", b"\xff\xd8\xffbody".as_slice()),
            ("image/gif", b"GIF89abody".as_slice()),
            ("image/webp", b"RIFF\0\0\0\0WEBP".as_slice()),
        ] {
            let attachment = MediaAttachment::image(mime_type, None, data.to_vec()).unwrap();
            onebot_image_segment(&attachment).unwrap();
        }

        let mut boundary = b"\x89PNG\r\n\x1a\n".to_vec();
        boundary.resize(MAX_INLINE_MEDIA_BYTES, 0);
        onebot_image_segment(&MediaAttachment::image("image/png", None, boundary).unwrap())
            .unwrap();

        assert!(MediaAttachment::image("image/jpeg", None, b"\x89PNG\r\n\x1a\n".to_vec()).is_err());
    }

    #[test]
    fn mapped_private_message_has_common_target() {
        let event = super::map_event(
            &AdapterId::new("onebot"),
            json!({
                "time":1,
                "self_id":2,
                "post_type":"message",
                "message_type":"private",
                "sub_type":"friend",
                "message_id":3,
                "user_id":4,
                "message":"hello",
                "raw_message":"hello",
                "sender":{}
            }),
        )
        .unwrap()
        .unwrap();
        let Event::Message(message) = event.event else {
            panic!("expected message");
        };
        assert_eq!(
            message.target,
            MessageTarget::Private {
                user_id: "4".to_owned()
            }
        );
    }
}
