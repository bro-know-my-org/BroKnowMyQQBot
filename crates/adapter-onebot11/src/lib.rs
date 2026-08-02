//! `OneBot` 11 reverse WebSocket adapter for `bot-core`.

#![forbid(unsafe_code)]

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use bot_core::{
    Action, ActionResult, Adapter, AdapterError, AdapterId, EventEnvelope, MessageTarget,
    ShutdownSignal,
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
    sync::{Mutex, mpsc, oneshot},
    task::JoinSet,
    time::{Instant, interval, timeout},
};
use tokio_tungstenite::{
    WebSocketStream, accept_hdr_async_with_config,
    tungstenite::{
        Message as WebSocketMessage, Utf8Bytes, handshake::server::ErrorResponse, http::StatusCode,
        protocol::WebSocketConfig,
    },
};
use tracing::{debug, info, warn};
use uuid::Uuid;

mod mapping;

pub use mapping::{MappingError, map_event};

const MAX_PENDING_HANDSHAKES: usize = 8;

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
}

struct PendingAction {
    deadline: Instant,
    response: oneshot::Sender<Result<ActionResponse, AdapterError>>,
}

pub struct OneBot11Adapter {
    config: OneBot11Config,
    local_addr: SocketAddr,
    listener: Mutex<Option<TcpListener>>,
    active: Mutex<Option<mpsc::Sender<OutboundAction>>>,
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
            active: Mutex::new(None),
        })
    }

    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    async fn serve_connection(
        &self,
        socket: WebSocketStream<TcpStream>,
        events: &mpsc::Sender<EventEnvelope>,
        shutdown: &mut ShutdownSignal,
    ) -> Result<(), AdapterError> {
        let (outbound_sender, outbound_receiver) = mpsc::channel(self.config.max_pending_actions);
        *self.active.lock().await = Some(outbound_sender);
        info!(adapter_id = %self.config.id, "OneBot 11 reverse WebSocket connected");
        let result = self
            .run_connection(socket, outbound_receiver, events, shutdown)
            .await;
        self.active.lock().await.take();
        info!(adapter_id = %self.config.id, "OneBot 11 reverse WebSocket disconnected");
        result
    }

    async fn run_connection(
        &self,
        socket: WebSocketStream<TcpStream>,
        mut outbound: mpsc::Receiver<OutboundAction>,
        events: &mpsc::Sender<EventEnvelope>,
        shutdown: &mut ShutdownSignal,
    ) -> Result<(), AdapterError> {
        let (mut writer, mut reader) = socket.split();
        let mut pending = HashMap::<String, PendingAction>::new();
        let mut deadline_tick = interval(Duration::from_millis(100));
        let result = loop {
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
                incoming = reader.next() => {
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
                            if let Err(error) = self.handle_text(&text, events, &mut pending) {
                                break Err(error);
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
        outbound.close();
        fail_unsent(&mut outbound);
        fail_pending_unknown(&mut pending);
        result
    }

    fn handle_text(
        &self,
        text: &Utf8Bytes,
        events: &mpsc::Sender<EventEnvelope>,
        pending: &mut HashMap<String, PendingAction>,
    ) -> Result<(), AdapterError> {
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
                    return Ok(());
                }
            };
            let Some(echo) = response.echo_key() else {
                warn!(adapter_id = %self.config.id, "ignored OneBot response with invalid echo");
                return Ok(());
            };
            if let Some(pending_action) = pending.remove(&echo) {
                let _ = pending_action.response.send(Ok(response));
            } else {
                debug!(adapter_id = %self.config.id, echo, "ignored unknown or duplicate OneBot echo");
            }
            return Ok(());
        }
        match map_event(&self.config.id, raw) {
            Ok(Some(event)) => match events.try_send(event) {
                Ok(()) => Ok(()),
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!(
                        adapter_id = %self.config.id,
                        "dropped OneBot event because the runtime queue is full"
                    );
                    Ok(())
                }
                Err(mpsc::error::TrySendError::Closed(_)) => Err(AdapterError::EventQueueClosed),
            },
            Ok(None) => Ok(()),
            Err(error) => {
                warn!(adapter_id = %self.config.id, error = %error, "ignored invalid OneBot event");
                Ok(())
            }
        }
    }

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
                    MessageTarget::Channel { .. } => Err(AdapterError::Action(
                        "OneBot 11 does not support the common channel target".to_owned(),
                    )),
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
                MessageTarget::Channel { .. } => Err(AdapterError::Action(
                    "OneBot 11 does not support the common channel target".to_owned(),
                )),
            },
            Action::Recall { message_id, .. } => Ok((
                "delete_msg".to_owned(),
                json!({"message_id": id_param(&message_id)}),
            )),
            Action::Platform { name, payload } if name == "onebot11.send_msg" => {
                Ok(("send_msg".to_owned(), payload))
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
        events: mpsc::Sender<EventEnvelope>,
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
                    let connection = self.serve_connection(socket, &events, &mut shutdown);
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
                    active_slot.store(false, Ordering::Release);
                }
            }
        }
    }

    async fn execute(&self, action: Action) -> Result<ActionResult, AdapterError> {
        let (action, params) = Self::action_request(action)?;
        let sender = self.active.lock().await.clone().ok_or_else(|| {
            AdapterError::Action("OneBot 11 reverse WebSocket is not connected".to_owned())
        })?;
        let (response_sender, response_receiver) = oneshot::channel();
        let deadline = Instant::now() + self.config.action_timeout;
        sender
            .try_send(OutboundAction {
                action,
                params,
                deadline,
                response: response_sender,
            })
            .map_err(|error| {
                AdapterError::Action(format!("OneBot Action queue unavailable: {error}"))
            })?;
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

fn id_param(value: &str) -> Value {
    Value::String(value.to_owned())
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
        Action, Adapter, AdapterId, Event, MessageTarget, ReplyAction, RuntimeBuilder,
        SendMessageAction, shutdown_channel,
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

    use super::{OneBot11Adapter, OneBot11Config};

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
        let (events, _received) = tokio::sync::mpsc::channel(1);
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
        let (events, _received) = tokio::sync::mpsc::channel(1);
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
        let (events, _received) = tokio::sync::mpsc::channel(1);
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
        let (events, _received) = tokio::sync::mpsc::channel(1);
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
    async fn action_timeout_is_reported_as_unknown() {
        let adapter = adapter_with_timeout(Duration::from_millis(100)).await;
        let (shutdown_handle, shutdown_signal) = shutdown_channel();
        let (events, _received) = tokio::sync::mpsc::channel(1);
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
        let (events, _received) = tokio::sync::mpsc::channel(1);
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
        let (events, _received) = tokio::sync::mpsc::channel(1);
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
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn malformed_response_like_json_does_not_close_connection() {
        let adapter = adapter().await;
        let (shutdown_handle, shutdown_signal) = shutdown_channel();
        let (events, _received) = tokio::sync::mpsc::channel(1);
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
        let (events, _received) = tokio::sync::mpsc::channel(1);
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
