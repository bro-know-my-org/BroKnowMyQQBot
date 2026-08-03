//! QQ Gateway WebSocket state machine and action executor.

use std::{
    collections::{BTreeMap, VecDeque},
    time::Duration,
};

use async_trait::async_trait;
use bot_core::{
    Action, ActionResult, Adapter, AdapterError, AdapterId, EventEnvelope, EventSendError,
    EventSender, MessageTarget, ShutdownSignal,
};
use futures_util::{SinkExt, StreamExt};
use qqbot_protocol::{
    ApiError, AuthError, ChannelMessageRequest, GatewayPayload, Intents, MediaFileType,
    MediaUploadRequest, MessageRequest, MessageResponse, OpCode, OpenApiClient,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    net::TcpStream,
    sync::Mutex,
    time::{Instant, MissedTickBehavior, interval_at, sleep, timeout},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async_with_config,
    tungstenite::{self, Message, protocol::WebSocketConfig},
};
use tracing::{debug, info, warn};

use crate::mapping::{MappingError, map_dispatch};

const HELLO_TIMEOUT: Duration = Duration::from_secs(15);
const MIN_RECONNECT_DELAY: Duration = Duration::from_millis(100);
const MAX_PENDING_DISPATCHES: usize = 1024;
const MAX_GATEWAY_MESSAGE_BYTES: usize = 1024 * 1024;
const MESSAGE_LOG_TARGET: &str = "bkm::messages";
type GatewaySocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Debug, Clone)]
pub struct QqWebSocketConfig {
    pub adapter_id: AdapterId,
    pub intents: Intents,
    pub shard: [u32; 2],
    pub log_message_content: bool,
    pub reconnect_min_delay: Duration,
    pub reconnect_max_delay: Duration,
}

impl Default for QqWebSocketConfig {
    fn default() -> Self {
        Self {
            adapter_id: AdapterId::new("qq-official-main"),
            intents: Intents::empty().with_group_and_c2c(),
            shard: [0, 1],
            log_message_content: false,
            reconnect_min_delay: Duration::from_secs(1),
            reconnect_max_delay: Duration::from_secs(30),
        }
    }
}

#[derive(Debug)]
pub struct QqWebSocketAdapter {
    config: QqWebSocketConfig,
    api: OpenApiClient,
    actions: QqActionExecutor,
    session: Mutex<SessionState>,
}

impl QqWebSocketAdapter {
    pub fn new(mut config: QqWebSocketConfig, api: OpenApiClient) -> Self {
        config.reconnect_min_delay = config.reconnect_min_delay.max(MIN_RECONNECT_DELAY);
        config.reconnect_max_delay = config.reconnect_max_delay.max(config.reconnect_min_delay);
        Self {
            actions: QqActionExecutor::new(
                config.adapter_id.clone(),
                api.clone(),
                config.log_message_content,
            ),
            config,
            api,
            session: Mutex::new(SessionState::default()),
        }
    }

    async fn run_forever(
        &self,
        events: EventSender,
        mut shutdown: ShutdownSignal,
    ) -> Result<(), AdapterError> {
        let mut backoff = ReconnectBackoff::default();
        loop {
            if shutdown.is_shutdown() {
                return Ok(());
            }
            events.mark_not_ready();
            let connection_result = self
                .run_connection(&events, &mut shutdown, &mut backoff)
                .await;
            events.mark_not_ready();
            match connection_result {
                Ok(ConnectionExit::Shutdown) => return Ok(()),
                Err(ConnectionError::EventAdapterMismatch { expected, actual }) => {
                    return Err(AdapterError::EventAdapterMismatch { expected, actual });
                }
                Err(ConnectionError::EventQueueClosed) => {
                    return Err(AdapterError::EventQueueClosed);
                }
                Err(error) if error.is_fatal() => {
                    return Err(AdapterError::Configuration(error.to_string()));
                }
                Ok(ConnectionExit::Reconnect) => {
                    warn!(adapter_id = %self.config.adapter_id, "QQ Gateway requested reconnect");
                }
                Err(error) => {
                    warn!(adapter_id = %self.config.adapter_id, error = %error, "QQ Gateway connection stopped");
                }
            }

            let delay = backoff.next_delay(
                self.config.reconnect_min_delay,
                self.config.reconnect_max_delay,
            );
            warn!(adapter_id = %self.config.adapter_id, delay_ms = delay.as_millis(), "retrying QQ Gateway connection after backoff");
            tokio::select! {
                () = shutdown.cancelled() => return Ok(()),
                () = sleep(delay) => {}
            }
        }
    }

    async fn run_connection(
        &self,
        events: &EventSender,
        shutdown: &mut ShutdownSignal,
        backoff: &mut ReconnectBackoff,
    ) -> Result<ConnectionExit, ConnectionError> {
        let Some((mut socket, heartbeat_duration)) = self.connect_authenticated(shutdown).await?
        else {
            return Ok(ConnectionExit::Shutdown);
        };
        let mut heartbeat = interval_at(Instant::now() + heartbeat_duration, heartbeat_duration);
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut heartbeat_acknowledged = true;

        loop {
            tokio::select! {
                () = shutdown.cancelled() => {
                    let _ = socket.close(None).await;
                    return Ok(ConnectionExit::Shutdown);
                }
                _ = heartbeat.tick() => {
                    if !heartbeat_acknowledged {
                        return Err(ConnectionError::HeartbeatAckMissing);
                    }
                    let seq = self.session.lock().await.last_received;
                    send_json(&mut socket, &OutboundPayload { op: OpCode::HEARTBEAT, d: seq }).await?;
                    heartbeat_acknowledged = false;
                }
                message = socket.next() => {
                    let Some(message) = message else {
                        return Err(ConnectionError::ConnectionClosed);
                    };
                    match message? {
                        Message::Text(text) => {
                            let payload: GatewayPayload = serde_json::from_str(text.as_str())?;
                            match payload.op {
                                OpCode::HEARTBEAT_ACK => heartbeat_acknowledged = true,
                                OpCode::HEARTBEAT => {
                                    let seq = self.session.lock().await.last_received;
                                    send_json(&mut socket, &OutboundPayload { op: OpCode::HEARTBEAT, d: seq }).await?;
                                    heartbeat_acknowledged = false;
                                }
                                OpCode::RECONNECT => return Ok(ConnectionExit::Reconnect),
                                OpCode::INVALID_SESSION => {
                                    self.session.lock().await.clear();
                                    return Ok(ConnectionExit::Reconnect);
                                }
                                OpCode::DISPATCH => {
                                    if self.handle_dispatch(payload, events).await? {
                                        backoff.connected();
                                    }
                                }
                                other => debug!(opcode = other.value(), "ignoring unsupported QQ Gateway opcode"),
                            }
                        }
                        Message::Binary(bytes) => {
                            let payload: GatewayPayload = serde_json::from_slice(&bytes)?;
                            match payload.op {
                                OpCode::HEARTBEAT_ACK => heartbeat_acknowledged = true,
                                OpCode::HEARTBEAT => {
                                    let seq = self.session.lock().await.last_received;
                                    send_json(&mut socket, &OutboundPayload { op: OpCode::HEARTBEAT, d: seq }).await?;
                                    heartbeat_acknowledged = false;
                                }
                                OpCode::RECONNECT => return Ok(ConnectionExit::Reconnect),
                                OpCode::INVALID_SESSION => {
                                    self.session.lock().await.clear();
                                    return Ok(ConnectionExit::Reconnect);
                                }
                                OpCode::DISPATCH => {
                                    if self.handle_dispatch(payload, events).await? {
                                        backoff.connected();
                                    }
                                }
                                other => debug!(opcode = other.value(), "ignoring unsupported QQ Gateway opcode"),
                            }
                        }
                        Message::Ping(payload) => socket.send(Message::Pong(payload)).await?,
                        Message::Pong(_) | Message::Frame(_) => {}
                        Message::Close(frame) => {
                            if let Some(frame) = frame {
                                let code: u16 = frame.code.into();
                                let reason = frame.reason.to_string();
                                warn!(adapter_id = %self.config.adapter_id, code, reason, "QQ Gateway closed WebSocket");
                                if matches!(code, 4006 | 4007 | 4009) {
                                    self.session.lock().await.clear();
                                    return Ok(ConnectionExit::Reconnect);
                                }
                                if is_fatal_close_code(code) {
                                    return Err(ConnectionError::FatalClose { code, reason });
                                }
                            }
                            return Err(ConnectionError::ConnectionClosed);
                        }
                    }
                }
            }
        }
    }

    async fn connect_authenticated(
        &self,
        shutdown: &mut ShutdownSignal,
    ) -> Result<Option<(GatewaySocket, Duration)>, ConnectionError> {
        let gateway = tokio::select! {
            () = shutdown.cancelled() => return Ok(None),
            gateway = self.api.gateway() => gateway?,
        };
        validate_gateway_url(&gateway.url)?;
        info!(adapter_id = %self.config.adapter_id, "connecting to QQ Gateway");
        let (mut socket, _) = tokio::select! {
            () = shutdown.cancelled() => return Ok(None),
            connected = timeout(
                HELLO_TIMEOUT,
                connect_async_with_config(
                    &gateway.url,
                    Some(
                        WebSocketConfig::default()
                            .max_message_size(Some(MAX_GATEWAY_MESSAGE_BYTES))
                            .max_frame_size(Some(MAX_GATEWAY_MESSAGE_BYTES)),
                    ),
                    false,
                ),
            ) => connected
                .map_err(|_| ConnectionError::ConnectTimeout)??,
        };

        let hello = tokio::select! {
            () = shutdown.cancelled() => return Ok(None),
            hello = timeout(HELLO_TIMEOUT, socket.next()) => hello
                .map_err(|_| ConnectionError::HelloTimeout)?
                .ok_or(ConnectionError::ClosedBeforeHello)??,
        };
        let hello = decode_message(hello)?;
        if hello.op != OpCode::HELLO {
            return Err(ConnectionError::UnexpectedOpcode {
                expected: OpCode::HELLO,
                actual: hello.op,
            });
        }
        let heartbeat_interval = hello
            .d
            .get("heartbeat_interval")
            .and_then(Value::as_u64)
            .filter(|interval| (1_000..=3_600_000).contains(interval))
            .ok_or(ConnectionError::InvalidHeartbeatInterval)?;

        let token = self.api.access_token().await?;
        let resume = self.session.lock().await.prepare_connection();
        if let Some(resume) = resume {
            send_json(
                &mut socket,
                &OutboundPayload {
                    op: OpCode::RESUME,
                    d: ResumeData {
                        token: format!("QQBot {}", token.expose()),
                        session_id: resume.session_id,
                        seq: resume.seq,
                    },
                },
            )
            .await?;
            debug!(adapter_id = %self.config.adapter_id, "sent QQ Gateway Resume");
        } else {
            send_json(
                &mut socket,
                &OutboundPayload {
                    op: OpCode::IDENTIFY,
                    d: IdentifyData {
                        token: format!("QQBot {}", token.expose()),
                        intents: self.config.intents,
                        shard: self.config.shard,
                        properties: BTreeMap::from([
                            ("$os", std::env::consts::OS),
                            ("$browser", "bro-know-my-qq-bot"),
                            ("$device", "bro-know-my-qq-bot"),
                        ]),
                    },
                },
            )
            .await?;
            debug!(adapter_id = %self.config.adapter_id, "sent QQ Gateway Identify");
        }
        Ok(Some((socket, Duration::from_millis(heartbeat_interval))))
    }

    #[allow(clippy::too_many_lines)]
    async fn handle_dispatch(
        &self,
        payload: GatewayPayload,
        events: &EventSender,
    ) -> Result<bool, ConnectionError> {
        let event_type = payload.t.as_deref().unwrap_or("UNKNOWN");
        let dispatch_event_id = payload.id.as_deref().unwrap_or("-");
        let seq = payload.s.unwrap_or_default();
        if event_type == "READY" {
            let session_id = payload
                .d
                .get("session_id")
                .and_then(Value::as_str)
                .ok_or(ConnectionError::MissingSessionId)?
                .to_owned();
            self.session.lock().await.session_id = Some(session_id);
            info!(
                adapter_id = %self.config.adapter_id,
                seq,
                "QQ Gateway connected"
            );
        } else if event_type == "RESUMED" {
            info!(
                adapter_id = %self.config.adapter_id,
                seq,
                "QQ Gateway session resumed"
            );
        }

        let authenticated = matches!(event_type, "READY" | "RESUMED");
        if authenticated {
            events.mark_ready();
        }
        let mut mapped = match map_dispatch(&self.config.adapter_id, &payload) {
            Ok(mapped) => mapped,
            Err(error) => {
                if let Some(seq) = payload.s {
                    let _ = self.session.lock().await.record_dispatch(seq, false);
                }
                warn!(adapter_id = %self.config.adapter_id, event_type, error = %error, "dropping malformed QQ Gateway dispatch");
                return Ok(authenticated);
            }
        };
        if let Some(seq) = payload.s {
            let record = self
                .session
                .lock()
                .await
                .record_dispatch(seq, mapped.is_some())?;
            match record {
                DispatchRecord::Accepted(delivery_id) => {
                    if let Some(event) = &mut mapped {
                        event.delivery_id = delivery_id;
                    }
                }
                DispatchRecord::Duplicate => return Ok(authenticated),
            }
        }
        if let Some(event) = mapped {
            let event_id = event.id.clone();
            let (message_scope, message_log) = match &event.event {
                bot_core::Event::Message(message) => (
                    message_scope_name(&message.target),
                    self.config
                        .log_message_content
                        .then(|| MessageLog::from(message)),
                ),
                _ => ("other", None),
            };
            if let Err(error) = events.try_send(event) {
                if let Some(seq) = payload.s {
                    self.session.lock().await.rollback_dispatch(seq);
                }
                return Err(match error {
                    EventSendError::QueueClosed => ConnectionError::EventQueueClosed,
                    EventSendError::QueueFull => ConnectionError::EventQueueBackpressure,
                    EventSendError::AdapterMismatch { expected, actual } => {
                        ConnectionError::EventAdapterMismatch { expected, actual }
                    }
                });
            }
            info!(
                adapter_id = %self.config.adapter_id,
                event_type = %event_type,
                message_scope = %message_scope,
                seq,
                "received QQ message event"
            );
            if let Some(message) = message_log {
                info!(
                    target: MESSAGE_LOG_TARGET,
                    adapter_id = %self.config.adapter_id,
                    event_id = %event_id,
                    event_type = %event_type,
                    message_scope = %message_scope,
                    direction = "inbound",
                    seq,
                    message_id = %message.message_id,
                    sender_id = %message.sender_id,
                    target_id = %message.target_id,
                    content = message.content,
                    "received QQ message content"
                );
            }
            debug!(
                adapter_id = %self.config.adapter_id,
                event_id = %compact_id(event_id.as_str()),
                event_type = %event_type,
                "QQ message event details"
            );
        } else if !matches!(event_type, "READY" | "RESUMED") {
            info!(
                adapter_id = %self.config.adapter_id,
                event_type = %event_type,
                seq,
                "received unmapped QQ Gateway event"
            );
            debug!(
                adapter_id = %self.config.adapter_id,
                event_id = %compact_id(dispatch_event_id),
                event_type = %event_type,
                "unmapped QQ Gateway event details"
            );
        }
        Ok(authenticated)
    }
}

#[derive(Debug)]
pub(crate) struct QqActionExecutor {
    adapter_id: AdapterId,
    api: OpenApiClient,
    log_message_content: bool,
}

#[derive(Debug, Deserialize)]
struct RichMessageAction {
    target: MessageTarget,
    body: Value,
    #[serde(default)]
    keyboard: Option<Value>,
    #[serde(default)]
    reply_to: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MediaUploadAction {
    target: MessageTarget,
    file_type: u8,
    url: String,
    #[serde(default)]
    send: bool,
}

#[derive(Debug, Deserialize)]
struct GuildAction {
    guild_id: String,
}

#[derive(Debug, Deserialize)]
struct ChannelAction {
    channel_id: String,
}

#[derive(Debug, Deserialize)]
struct CreateChannelAction {
    guild_id: String,
    body: Value,
}

#[derive(Debug, Deserialize)]
struct UpdateChannelAction {
    channel_id: String,
    body: Value,
}

impl QqActionExecutor {
    pub(crate) fn new(
        adapter_id: AdapterId,
        api: OpenApiClient,
        log_message_content: bool,
    ) -> Self {
        Self {
            adapter_id,
            api,
            log_message_content,
        }
    }

    pub(crate) async fn execute_action(
        &self,
        action: Action,
    ) -> Result<ActionResult, AdapterError> {
        match action {
            Action::Reply(reply) => {
                let message_scope = message_scope_name(&reply.target);
                let message_log = self
                    .log_message_content
                    .then(|| MessageActionLog::new(&reply.target, reply.content.clone()));
                let result = match &reply.target {
                    MessageTarget::Group { group_id } => {
                        self.api
                            .send_group_message(
                                group_id,
                                &MessageRequest::reply_text(
                                    &reply.source_message_id,
                                    &reply.content,
                                ),
                            )
                            .await
                    }
                    MessageTarget::Private { user_id } => {
                        self.api
                            .send_c2c_message(
                                user_id,
                                &MessageRequest::reply_text(
                                    &reply.source_message_id,
                                    &reply.content,
                                ),
                            )
                            .await
                    }
                    MessageTarget::Channel { channel_id } => {
                        self.api
                            .send_channel_message(
                                channel_id,
                                &ChannelMessageRequest::text(reply.content)
                                    .with_reply_to(reply.source_message_id),
                            )
                            .await
                    }
                };
                self.complete_message_action("message.reply", message_scope, result, message_log)
            }
            Action::SendMessage(message) => {
                let message_scope = message_scope_name(&message.target);
                let message_log = self
                    .log_message_content
                    .then(|| MessageActionLog::new(&message.target, message.content.clone()));
                let result = match &message.target {
                    MessageTarget::Group { group_id } => {
                        self.api
                            .send_group_message(group_id, &MessageRequest::text(&message.content))
                            .await
                    }
                    MessageTarget::Private { user_id } => {
                        self.api
                            .send_c2c_message(user_id, &MessageRequest::text(&message.content))
                            .await
                    }
                    MessageTarget::Channel { channel_id } => {
                        self.api
                            .send_channel_message(
                                channel_id,
                                &ChannelMessageRequest::text(message.content),
                            )
                            .await
                    }
                };
                self.complete_message_action("message.send", message_scope, result, message_log)
            }
            Action::Recall { target, message_id } => {
                let result = match &target {
                    MessageTarget::Group { group_id } => {
                        self.api.recall_group_message(group_id, &message_id).await
                    }
                    MessageTarget::Private { user_id } => {
                        self.api.recall_c2c_message(user_id, &message_id).await
                    }
                    MessageTarget::Channel { channel_id } => {
                        self.api
                            .recall_channel_message(channel_id, &message_id)
                            .await
                    }
                };
                self.complete_unit_action("message.recall", message_scope_name(&target), result)
            }
            Action::Platform { name, payload } => {
                self.execute_platform_action(&name, payload).await
            }
        }
    }

    async fn execute_platform_action(
        &self,
        name: &str,
        payload: Value,
    ) -> Result<ActionResult, AdapterError> {
        match name {
            "qq.message.markdown" | "qq.message.ark" => {
                self.execute_rich_message(name, payload).await
            }
            "qq.media.upload" => self.execute_media_upload(payload).await,
            "qq.guild.list" | "qq.guild.get" | "qq.channel.list" | "qq.channel.get"
            | "qq.channel.create" | "qq.channel.update" | "qq.channel.delete" => {
                self.execute_channel_action(name, payload).await
            }
            _ => Err(AdapterError::Action(format!(
                "unsupported QQ platform Action `{name}`"
            ))),
        }
    }

    async fn execute_rich_message(
        &self,
        name: &str,
        payload: Value,
    ) -> Result<ActionResult, AdapterError> {
        let action: RichMessageAction = decode_platform_payload(name, payload)?;
        require_object(name, "body", &action.body)?;
        if let Some(keyboard) = &action.keyboard {
            require_object(name, "keyboard", keyboard)?;
        }
        if name == "qq.message.ark" && action.keyboard.is_some() {
            return Err(AdapterError::Action(
                "qq.message.ark does not accept keyboard".to_owned(),
            ));
        }
        let message_scope = message_scope_name(&action.target);
        let RichMessageAction {
            target,
            body,
            keyboard,
            reply_to,
        } = action;
        let result = match target {
            MessageTarget::Group { group_id } => {
                let request = if name == "qq.message.markdown" {
                    MessageRequest::markdown(body, keyboard)
                } else {
                    MessageRequest::ark(body)
                };
                let request = match reply_to.as_deref() {
                    Some(reply) => request.with_reply_to(reply),
                    None => request,
                };
                self.api.send_group_message(&group_id, &request).await
            }
            MessageTarget::Private { user_id } => {
                let request = if name == "qq.message.markdown" {
                    MessageRequest::markdown(body, keyboard)
                } else {
                    MessageRequest::ark(body)
                };
                let request = match reply_to.as_deref() {
                    Some(reply) => request.with_reply_to(reply),
                    None => request,
                };
                self.api.send_c2c_message(&user_id, &request).await
            }
            MessageTarget::Channel { channel_id } => {
                let request = if name == "qq.message.markdown" {
                    ChannelMessageRequest::markdown(body, keyboard)
                } else {
                    ChannelMessageRequest::ark(body)
                };
                let request = match reply_to.as_deref() {
                    Some(reply) => request.with_reply_to(reply),
                    None => request,
                };
                self.api.send_channel_message(&channel_id, &request).await
            }
        };
        self.complete_message_action(
            if name == "qq.message.markdown" {
                "qq.message.markdown"
            } else {
                "qq.message.ark"
            },
            message_scope,
            result,
            None,
        )
    }

    async fn execute_media_upload(&self, payload: Value) -> Result<ActionResult, AdapterError> {
        let action: MediaUploadAction = decode_platform_payload("qq.media.upload", payload)?;
        let file_type = MediaFileType::try_from(action.file_type)
            .map_err(|message| AdapterError::Action(message.to_owned()))?;
        let mut request = MediaUploadRequest::from_url(file_type, action.url);
        request.srv_send_msg = action.send;
        let response = match &action.target {
            MessageTarget::Group { group_id } => {
                self.api.upload_group_media(group_id, &request).await
            }
            MessageTarget::Private { user_id } => {
                self.api.upload_c2c_media(user_id, &request).await
            }
            MessageTarget::Channel { .. } => {
                return Err(AdapterError::Action(
                    "QQ channel media upload is not supported by this endpoint".to_owned(),
                ));
            }
        }
        .map_err(|error| map_action_error(&error))?;
        Ok(ActionResult {
            message_id: None,
            raw: serde_json::to_value(response).unwrap_or(Value::Null),
        })
    }

    async fn execute_channel_action(
        &self,
        name: &str,
        payload: Value,
    ) -> Result<ActionResult, AdapterError> {
        match name {
            "qq.guild.list" => self.complete_value_action(name, self.api.guilds().await),
            "qq.guild.get" => {
                let action: GuildAction = decode_platform_payload(name, payload)?;
                self.complete_value_action(name, self.api.guild(&action.guild_id).await)
            }
            "qq.channel.list" => {
                let action: GuildAction = decode_platform_payload(name, payload)?;
                self.complete_value_action(name, self.api.guild_channels(&action.guild_id).await)
            }
            "qq.channel.get" => {
                let action: ChannelAction = decode_platform_payload(name, payload)?;
                self.complete_value_action(name, self.api.channel(&action.channel_id).await)
            }
            "qq.channel.create" => {
                let action: CreateChannelAction = decode_platform_payload(name, payload)?;
                require_object(name, "body", &action.body)?;
                self.complete_value_action(
                    name,
                    self.api
                        .create_channel_raw(&action.guild_id, &action.body)
                        .await,
                )
            }
            "qq.channel.update" => {
                let action: UpdateChannelAction = decode_platform_payload(name, payload)?;
                require_object(name, "body", &action.body)?;
                self.complete_value_action(
                    name,
                    self.api
                        .update_channel_raw(&action.channel_id, &action.body)
                        .await,
                )
            }
            "qq.channel.delete" => {
                let action: ChannelAction = decode_platform_payload(name, payload)?;
                self.complete_unit_action(
                    "qq.channel.delete",
                    "channel",
                    self.api.delete_channel(&action.channel_id).await,
                )
            }
            _ => unreachable!("channel Action dispatcher only calls known names"),
        }
    }

    fn complete_unit_action(
        &self,
        action_type: &'static str,
        message_scope: &'static str,
        result: Result<(), ApiError>,
    ) -> Result<ActionResult, AdapterError> {
        match result {
            Ok(()) => {
                info!(adapter_id = %self.adapter_id, action_type, message_scope, "QQ platform action succeeded");
                Ok(ActionResult::default())
            }
            Err(error) => {
                warn!(adapter_id = %self.adapter_id, action_type, message_scope, error = %error, "QQ platform action failed");
                Err(map_action_error(&error))
            }
        }
    }

    fn complete_value_action(
        &self,
        action_type: &str,
        result: Result<Value, ApiError>,
    ) -> Result<ActionResult, AdapterError> {
        match result {
            Ok(raw) => {
                info!(adapter_id = %self.adapter_id, action_type, "QQ platform action succeeded");
                Ok(ActionResult {
                    message_id: None,
                    raw,
                })
            }
            Err(error) => {
                warn!(adapter_id = %self.adapter_id, action_type, error = %error, "QQ platform action failed");
                Err(map_action_error(&error))
            }
        }
    }

    fn complete_message_action(
        &self,
        action_type: &'static str,
        message_scope: &'static str,
        result: Result<MessageResponse, ApiError>,
        message_log: Option<MessageActionLog>,
    ) -> Result<ActionResult, AdapterError> {
        match result {
            Ok(result) => {
                info!(
                    adapter_id = %self.adapter_id,
                    action_type = %action_type,
                    message_scope = %message_scope,
                    "QQ message action succeeded"
                );
                if let Some(message) = message_log {
                    info!(
                        target: MESSAGE_LOG_TARGET,
                        adapter_id = %self.adapter_id,
                        action_type = %action_type,
                        message_scope = %message_scope,
                        direction = "outbound",
                        status = "succeeded",
                        message_id = %result.id.as_deref().unwrap_or("-"),
                        target_id = %message.target_id,
                        content = message.content,
                        "sent QQ message content"
                    );
                }
                debug!(
                    adapter_id = %self.adapter_id,
                    action_type = %action_type,
                    message_id = %compact_id(result.id.as_deref().unwrap_or("-")),
                    "QQ message action result details"
                );
                Ok(ActionResult {
                    message_id: result.id.clone(),
                    raw: serde_json::to_value(result).unwrap_or(Value::Null),
                })
            }
            Err(error) => {
                warn!(
                    adapter_id = %self.adapter_id,
                    action_type = %action_type,
                    message_scope = %message_scope,
                    error = %error,
                    "QQ message action failed"
                );
                if let Some(message) = message_log {
                    info!(
                        target: MESSAGE_LOG_TARGET,
                        adapter_id = %self.adapter_id,
                        action_type = %action_type,
                        message_scope = %message_scope,
                        direction = "outbound",
                        status = "failed",
                        target_id = %message.target_id,
                        content = message.content,
                        error = %error,
                        "failed to send QQ message content"
                    );
                }
                Err(map_action_error(&error))
            }
        }
    }
}

fn decode_platform_payload<T>(name: &str, payload: Value) -> Result<T, AdapterError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_value(payload)
        .map_err(|error| AdapterError::Action(format!("invalid {name} payload: {error}")))
}

fn require_object(name: &str, field: &str, value: &Value) -> Result<(), AdapterError> {
    value.is_object().then_some(()).ok_or_else(|| {
        AdapterError::Action(format!(
            "invalid {name} payload: {field} must be a JSON object"
        ))
    })
}

const fn message_scope_name(target: &MessageTarget) -> &'static str {
    match target {
        MessageTarget::Group { .. } => "group",
        MessageTarget::Private { .. } => "private",
        MessageTarget::Channel { .. } => "channel",
    }
}

pub(crate) fn compact_id(value: &str) -> String {
    const EDGE_LENGTH: usize = 8;
    let characters = value.chars().collect::<Vec<_>>();
    if characters.len() <= EDGE_LENGTH * 2 {
        return value.to_owned();
    }
    let prefix = characters[..EDGE_LENGTH].iter().collect::<String>();
    let suffix = characters[characters.len() - EDGE_LENGTH..]
        .iter()
        .collect::<String>();
    format!("{prefix}…{suffix}")
}

#[derive(Debug)]
struct MessageLog {
    message_id: String,
    sender_id: String,
    target_id: String,
    content: String,
}

#[derive(Debug)]
struct MessageActionLog {
    target_id: String,
    content: String,
}

impl MessageActionLog {
    fn new(target: &MessageTarget, content: String) -> Self {
        let target_id = match target {
            MessageTarget::Group { group_id } => group_id.clone(),
            MessageTarget::Private { user_id } => user_id.clone(),
            MessageTarget::Channel { channel_id } => channel_id.clone(),
        };
        Self { target_id, content }
    }
}

impl From<&bot_core::CommonMessage> for MessageLog {
    fn from(message: &bot_core::CommonMessage) -> Self {
        let target_id = match &message.target {
            MessageTarget::Group { group_id } => group_id.clone(),
            MessageTarget::Private { user_id } => user_id.clone(),
            MessageTarget::Channel { channel_id } => channel_id.clone(),
        };
        Self {
            message_id: message.message_id.clone(),
            sender_id: message.sender.id.clone(),
            target_id,
            content: message.text.clone(),
        }
    }
}

fn map_action_error(error: &ApiError) -> AdapterError {
    match error {
        ApiError::Request(_) | ApiError::Decode(_) => {
            AdapterError::ActionUnknown(error.to_string())
        }
        ApiError::Authentication(_)
        | ApiError::HttpStatus { .. }
        | ApiError::Platform { .. }
        | ApiError::ResponseTooLarge
        | ApiError::InvalidRequest(_)
        | ApiError::InvalidUrl(_) => AdapterError::Action(error.to_string()),
    }
}

#[async_trait]
impl Adapter for QqWebSocketAdapter {
    fn id(&self) -> &AdapterId {
        &self.config.adapter_id
    }

    fn platform(&self) -> &'static str {
        "qq.official"
    }

    async fn run(&self, events: EventSender, shutdown: ShutdownSignal) -> Result<(), AdapterError> {
        self.run_forever(events, shutdown).await
    }

    async fn execute(&self, action: Action) -> Result<ActionResult, AdapterError> {
        self.actions.execute_action(action).await
    }

    async fn event_handled(&self, event: &EventEnvelope) -> Result<(), AdapterError> {
        if let Some(delivery_id) = event.delivery_id {
            self.session.lock().await.mark_handled(delivery_id);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectionExit {
    Shutdown,
    Reconnect,
}

#[derive(Debug, thiserror::Error)]
enum ConnectionError {
    #[error(transparent)]
    Api(#[from] qqbot_protocol::ApiError),
    #[error(transparent)]
    WebSocket(#[from] tungstenite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Mapping(#[from] MappingError),
    #[error("QQ Gateway did not send Hello before timeout")]
    HelloTimeout,
    #[error("QQ Gateway connection attempt timed out")]
    ConnectTimeout,
    #[error("invalid QQ Gateway URL: {0}")]
    InvalidGatewayUrl(String),
    #[error("QQ Gateway closed before Hello")]
    ClosedBeforeHello,
    #[error("expected QQ Gateway opcode {expected:?}, received {actual:?}")]
    UnexpectedOpcode { expected: OpCode, actual: OpCode },
    #[error("QQ Gateway Hello has a missing or zero heartbeat_interval")]
    InvalidHeartbeatInterval,
    #[error("QQ Gateway Ready is missing session_id")]
    MissingSessionId,
    #[error("QQ Gateway heartbeat ACK was not received")]
    HeartbeatAckMissing,
    #[error("QQ Gateway connection closed")]
    ConnectionClosed,
    #[error("runtime event queue is closed")]
    EventQueueClosed,
    #[error("runtime event queue is full; reconnecting so the event can be replayed")]
    EventQueueBackpressure,
    #[error("event Adapter mismatch: expected `{expected}`, received `{actual}`")]
    EventAdapterMismatch {
        expected: AdapterId,
        actual: AdapterId,
    },
    #[error("too many QQ Gateway events are awaiting successful handler completion")]
    UncommittedEventLimit,
    #[error("QQ Gateway dispatch sequence decreased from {last} to {received}")]
    InvalidDispatchSequence { last: u64, received: u64 },
    #[error("QQ Gateway closed with non-retryable code {code}: {reason}")]
    FatalClose { code: u16, reason: String },
}

fn validate_gateway_url(value: &str) -> Result<(), ConnectionError> {
    let url = url::Url::parse(value)
        .map_err(|error| ConnectionError::InvalidGatewayUrl(error.to_string()))?;
    let loopback = url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    });
    if url.scheme() != "wss" && !(url.scheme() == "ws" && loopback) {
        return Err(ConnectionError::InvalidGatewayUrl(
            "URL must use wss".to_owned(),
        ));
    }
    let trusted_qq_host = url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("qq.com")
            || host.ends_with(".qq.com")
            || host.eq_ignore_ascii_case("sgroup.qq.com")
            || host.ends_with(".sgroup.qq.com")
    });
    if !loopback && !trusted_qq_host {
        return Err(ConnectionError::InvalidGatewayUrl(
            "host is not an approved QQ Gateway domain".to_owned(),
        ));
    }
    Ok(())
}

impl ConnectionError {
    fn is_fatal(&self) -> bool {
        match self {
            Self::FatalClose { .. }
            | Self::InvalidGatewayUrl(_)
            | Self::EventAdapterMismatch { .. } => true,
            Self::Api(error) => is_fatal_api_error(error),
            _ => false,
        }
    }
}

#[derive(Debug, Default)]
struct ReconnectBackoff {
    attempt: u32,
}

impl ReconnectBackoff {
    fn connected(&mut self) {
        self.attempt = 0;
    }

    fn next_delay(&mut self, minimum: Duration, maximum: Duration) -> Duration {
        let delay = reconnect_delay(minimum, maximum, self.attempt);
        self.attempt = self.attempt.saturating_add(1);
        delay
    }
}

#[derive(Debug, Default)]
struct SessionState {
    session_id: Option<String>,
    last_received: Option<u64>,
    last_committed: Option<u64>,
    pending: VecDeque<PendingDispatch>,
    next_delivery_id: u64,
}

impl SessionState {
    fn prepare_connection(&mut self) -> Option<ResumeState> {
        Some(ResumeState {
            session_id: self.session_id.clone()?,
            seq: self.last_committed?,
        })
    }

    fn record_dispatch(
        &mut self,
        seq: u64,
        has_event: bool,
    ) -> Result<DispatchRecord, ConnectionError> {
        if self
            .last_committed
            .is_some_and(|committed| seq <= committed)
            || self.pending.iter().any(|dispatch| dispatch.seq == seq)
        {
            return Ok(DispatchRecord::Duplicate);
        }
        if let Some(last) = self.last_received {
            if seq < last {
                return Err(ConnectionError::InvalidDispatchSequence {
                    last,
                    received: seq,
                });
            }
        }
        if has_event && self.pending.len() >= MAX_PENDING_DISPATCHES {
            return Err(ConnectionError::UncommittedEventLimit);
        }
        let delivery_id = has_event.then(|| {
            self.next_delivery_id = self.next_delivery_id.wrapping_add(1).max(1);
            self.next_delivery_id
        });
        self.last_received = Some(seq);
        self.pending.push_back(PendingDispatch {
            seq,
            completed: !has_event,
            delivery_id,
        });
        self.advance_committed();
        Ok(DispatchRecord::Accepted(delivery_id))
    }

    fn mark_handled(&mut self, delivery_id: u64) {
        if let Some(dispatch) = self
            .pending
            .iter_mut()
            .find(|dispatch| dispatch.delivery_id == Some(delivery_id))
        {
            dispatch.completed = true;
        }
        self.advance_committed();
    }

    fn rollback_dispatch(&mut self, seq: u64) {
        self.pending.retain(|dispatch| dispatch.seq != seq);
        self.last_received = self
            .pending
            .back()
            .map(|dispatch| dispatch.seq)
            .or(self.last_committed);
    }

    fn advance_committed(&mut self) {
        while self.pending.front().is_some_and(|item| item.completed) {
            if let Some(item) = self.pending.pop_front() {
                self.last_committed = Some(item.seq);
            }
        }
    }

    fn clear(&mut self) {
        let next_delivery_id = self.next_delivery_id;
        *self = Self {
            next_delivery_id,
            ..Self::default()
        };
    }
}

#[derive(Debug)]
struct PendingDispatch {
    seq: u64,
    delivery_id: Option<u64>,
    completed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DispatchRecord {
    Accepted(Option<u64>),
    Duplicate,
}

#[derive(Debug)]
struct ResumeState {
    session_id: String,
    seq: u64,
}

#[derive(Serialize)]
struct OutboundPayload<T> {
    op: OpCode,
    d: T,
}

#[derive(Serialize)]
struct IdentifyData<'a> {
    token: String,
    intents: Intents,
    shard: [u32; 2],
    properties: BTreeMap<&'a str, &'a str>,
}

#[derive(Serialize)]
struct ResumeData {
    token: String,
    session_id: String,
    seq: u64,
}

async fn send_json<S, T>(
    socket: &mut tokio_tungstenite::WebSocketStream<S>,
    value: &T,
) -> Result<(), ConnectionError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    T: Serialize,
{
    let text = serde_json::to_string(value)?;
    socket.send(Message::Text(text.into())).await?;
    Ok(())
}

fn decode_message(message: Message) -> Result<GatewayPayload, ConnectionError> {
    match message {
        Message::Text(text) => Ok(serde_json::from_str(text.as_str())?),
        Message::Binary(bytes) => Ok(serde_json::from_slice(&bytes)?),
        _ => Err(ConnectionError::ClosedBeforeHello),
    }
}

fn is_fatal_close_code(code: u16) -> bool {
    matches!(
        code,
        4001 | 4002 | 4003 | 4004 | 4005 | 4010 | 4011 | 4012 | 4013 | 4014 | 4914 | 4915
    )
}

fn is_fatal_api_error(error: &ApiError) -> bool {
    match error {
        ApiError::InvalidUrl(_) | ApiError::InvalidRequest(_) => true,
        ApiError::Authentication(AuthError::HttpStatus { status })
        | ApiError::HttpStatus { status, .. } => status.is_client_error() && status.as_u16() != 429,
        ApiError::Authentication(AuthError::Request(_) | AuthError::InvalidResponse(_))
        | ApiError::Request(_)
        | ApiError::Platform { .. }
        | ApiError::ResponseTooLarge
        | ApiError::Decode(_) => false,
    }
}

fn reconnect_delay(minimum: Duration, maximum: Duration, attempt: u32) -> Duration {
    let exponent = attempt.min(10);
    let multiplier = 1_u32 << exponent;
    let base = minimum.saturating_mul(multiplier).min(maximum);
    let jitter_millis = (u64::from(attempt).wrapping_mul(1_103_515_245) + 12_345) % 251;
    base.saturating_add(Duration::from_millis(jitter_millis))
        .min(maximum)
}

#[cfg(test)]
mod tests {
    use qqbot_protocol::{ApiError, AuthError};
    use serde_json::json;

    use super::{
        DispatchRecord, ReconnectBackoff, SessionState, is_fatal_api_error, require_object,
    };

    #[test]
    fn platform_document_fields_require_json_objects() {
        require_object("qq.message.markdown", "body", &json!({"content":"ok"})).unwrap();
        assert!(require_object("qq.message.markdown", "body", &json!(null)).is_err());
        assert!(require_object("qq.channel.create", "body", &json!(["invalid"])).is_err());
        assert!(require_object("qq.message.markdown", "keyboard", &json!("bad")).is_err());
    }

    #[test]
    fn committed_sequence_only_advances_across_contiguous_completed_events() {
        let mut state = SessionState::default();
        state.record_dispatch(1, false).unwrap();
        let DispatchRecord::Accepted(Some(first_delivery)) =
            state.record_dispatch(2, true).unwrap()
        else {
            panic!("first event must be accepted")
        };
        let DispatchRecord::Accepted(Some(second_delivery)) =
            state.record_dispatch(3, true).unwrap()
        else {
            panic!("second event must be accepted")
        };

        state.mark_handled(second_delivery);
        assert_eq!(state.last_committed, Some(1));
        state.mark_handled(first_delivery);
        assert_eq!(state.last_committed, Some(3));
    }

    #[test]
    fn acknowledgement_cannot_complete_a_replayed_delivery() {
        let mut state = SessionState {
            session_id: Some("session".to_owned()),
            last_committed: Some(1),
            ..SessionState::default()
        };
        let DispatchRecord::Accepted(Some(old_delivery)) = state.record_dispatch(2, true).unwrap()
        else {
            panic!("original event must be accepted")
        };
        state.prepare_connection();
        assert_eq!(
            state.record_dispatch(2, true).unwrap(),
            DispatchRecord::Duplicate
        );

        state.mark_handled(old_delivery);
        assert_eq!(state.last_committed, Some(2));
    }

    #[test]
    fn failed_queue_admission_allows_gateway_replay() {
        let mut state = SessionState {
            session_id: Some("session".to_owned()),
            last_committed: Some(1),
            ..SessionState::default()
        };
        let DispatchRecord::Accepted(Some(first_delivery)) =
            state.record_dispatch(2, true).unwrap()
        else {
            panic!("original event must be accepted")
        };
        state.rollback_dispatch(2);

        let DispatchRecord::Accepted(Some(replayed_delivery)) =
            state.record_dispatch(2, true).unwrap()
        else {
            panic!("replayed event must be accepted after rollback")
        };
        assert_ne!(first_delivery, replayed_delivery);
    }

    #[test]
    fn authenticated_connection_resets_exponential_backoff() {
        let minimum = std::time::Duration::from_secs(1);
        let maximum = std::time::Duration::from_secs(30);
        let mut backoff = ReconnectBackoff::default();
        let first = backoff.next_delay(minimum, maximum);
        assert!(backoff.next_delay(minimum, maximum) > first);
        backoff.connected();
        assert_eq!(backoff.next_delay(minimum, maximum), first);
    }

    #[test]
    fn deterministic_client_auth_errors_are_fatal_but_rate_limits_are_retryable() {
        assert!(is_fatal_api_error(&ApiError::Authentication(
            AuthError::HttpStatus {
                status: reqwest::StatusCode::UNAUTHORIZED,
            },
        )));
        assert!(!is_fatal_api_error(&ApiError::HttpStatus {
            status: reqwest::StatusCode::TOO_MANY_REQUESTS,
            code: None,
            message: None,
            trace_id: None,
            retry_after: Some(std::time::Duration::from_secs(1)),
        }));
    }
}
