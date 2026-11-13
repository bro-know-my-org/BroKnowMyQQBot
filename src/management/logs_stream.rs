//! Bounded SSE log delivery. The response owns its slot and all waiting state.

use std::{convert::Infallible, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
    middleware,
    response::{
        IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
    routing::get,
};
use bot_core::ShutdownSignal;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};

use super::{
    auth::{self, AuthState},
    logs::LogQuery,
};
use crate::logging::live::LogBuffer;

const IDLE_CHECK: Duration = Duration::from_secs(15);
const MAX_STREAMS: usize = 16;
const MIN_PAGE_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Clone)]
struct StreamContext {
    logs: LogBuffer,
    auth: AuthState,
    slots: Arc<Semaphore>,
    shutdown: Option<ShutdownSignal>,
}

pub(super) fn router(logs: LogBuffer, auth: AuthState, shutdown: Option<ShutdownSignal>) -> Router {
    Router::new()
        .route("/api/logs/stream", get(stream))
        .with_state(StreamContext {
            logs,
            auth: auth.clone(),
            slots: Arc::new(Semaphore::new(MAX_STREAMS)),
            shutdown,
        })
        .route_layer(middleware::from_fn(super::plugins::read_body))
        .route_layer(middleware::from_fn_with_state(auth, auth::authenticate))
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct StreamQuery {
    after: Option<u64>,
    session_id: Option<String>,
    limit: Option<usize>,
    level: Option<String>,
    instance_id: Option<String>,
}

fn valid_session(session: &str) -> bool {
    !session.is_empty()
        && session.len() <= 128
        && session
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

impl StreamQuery {
    fn resolve(mut self, headers: &HeaderMap) -> Option<(LogQuery, Option<String>)> {
        // Validate explicit resume parameters even when Last-Event-ID supersedes them.
        if self.after.is_some() != self.session_id.is_some()
            || self
                .session_id
                .as_deref()
                .is_some_and(|id| !valid_session(id))
        {
            return None;
        }
        if headers.contains_key("last-event-id") {
            let mut values = headers.get_all("last-event-id").iter();
            let value = values.next()?.to_str().ok()?;
            if values.next().is_some() {
                return None;
            }
            let (session, cursor) = value.split_once(':')?;
            if !valid_session(session)
                || cursor.is_empty()
                || !cursor.bytes().all(|b| b.is_ascii_digit())
            {
                return None;
            }
            self.session_id = Some(session.to_owned());
            self.after = Some(cursor.parse().ok()?);
        }
        let query = LogQuery {
            after: self.after,
            limit: self.limit,
            level: self.level,
            instance_id: self.instance_id,
        };
        query.valid().then_some((query, self.session_id))
    }
}

async fn stream(
    State(context): State<StreamContext>,
    Query(query): Query<StreamQuery>,
    headers: HeaderMap,
) -> Response {
    let Some((query, session_id)) = query.resolve(&headers) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"日志筛选或续接参数无效"})),
        )
            .into_response();
    };
    let Ok(permit) = context.slots.clone().try_acquire_owned() else {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error":"日志订阅已达上限"})),
        )
            .into_response();
    };
    // Subscribe before the first snapshot, including when the body is polled later.
    let updates = context.logs.subscribe();
    let first = query.after.is_none();
    let state = StreamState {
        context,
        query,
        session_id,
        headers,
        updates,
        permit: Some(permit),
        first,
        next_page_at: None,
    };
    let events = futures_util::stream::unfold(state, |mut state| async move {
        let event = state.next().await?;
        Some((Ok::<_, Infallible>(event), state))
    });
    let mut response = Sse::new(events)
        .keep_alive(KeepAlive::new().interval(IDLE_CHECK))
        .into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        "no-store".parse().expect("static header"),
    );
    response
        .headers_mut()
        .insert("x-accel-buffering", "no".parse().expect("static header"));
    response
}

struct StreamState {
    context: StreamContext,
    query: LogQuery,
    session_id: Option<String>,
    headers: HeaderMap,
    updates: watch::Receiver<u64>,
    permit: Option<OwnedSemaphorePermit>,
    first: bool,
    next_page_at: Option<tokio::time::Instant>,
}

impl StreamState {
    async fn next(&mut self) -> Option<Event> {
        loop {
            if self.permit.is_none()
                || self
                    .context
                    .shutdown
                    .as_ref()
                    .is_some_and(ShutdownSignal::is_shutdown)
            {
                self.permit.take();
                return None;
            }
            if !self.context.auth.authenticated(&self.headers) {
                self.permit.take();
                return Some(Event::default().event("auth-required").data("{}"));
            }
            // Coalesce bursts against the bounded ring instead of sending one
            // SSE frame per tracing event to each subscriber.
            if let Some(deadline) = self.next_page_at.take() {
                tokio::time::sleep_until(deadline).await;
                continue;
            }
            // Mark the notification seen before snapshotting: writes during or
            // after the snapshot remain observable by changed().
            self.updates.borrow_and_update();
            let page = self.context.logs.stream_snapshot(
                self.query.after,
                self.session_id.as_deref(),
                self.query.limit.unwrap_or(200),
                self.query.level.as_deref(),
                self.query.instance_id.as_deref(),
            );
            if self.first || page.dropped || self.query.after != Some(page.next_cursor) {
                self.first = false;
                self.query.after = Some(page.next_cursor);
                self.session_id = Some(page.session_id.clone());
                self.next_page_at = Some(tokio::time::Instant::now() + MIN_PAGE_INTERVAL);
                // Each call immediately snapshots the next page when has_more;
                // otherwise it waits below once the cursor reaches the tail.
                return Some(
                    Event::default()
                        .event("logs")
                        .id(format!("{}:{}", page.session_id, page.next_cursor))
                        .json_data(page)
                        .expect("log page serializes"),
                );
            }
            tokio::select! {
                biased;
                () = async {
                    if let Some(shutdown) = &mut self.context.shutdown {
                        shutdown.cancelled().await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {
                    self.permit.take();
                    return None;
                }
                result = self.updates.changed() => {
                    if result.is_err() { return None; }
                }
                () = tokio::time::sleep(IDLE_CHECK) => {}
            }
        }
    }
}
