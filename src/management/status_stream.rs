//! Shared, bounded status sampling; each response owns only its cursor and stream slot.
use super::auth::{self, AuthState};
use crate::plugin_management::PluginManager;
use axum::{
    Extension, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    middleware,
    response::{
        IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
    routing::get,
};
use bot_core::{RuntimeObserver, ShutdownSignal};
use serde_json::{Value, json};
use std::{convert::Infallible, sync::Arc, time::Duration};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot, watch};

const MAX_STREAMS: usize = 16;
const COALESCE: Duration = Duration::from_millis(250);
const SAMPLE: Duration = Duration::from_secs(2);
const HEARTBEAT: Duration = Duration::from_secs(15);

#[derive(Clone)]
struct Frame {
    revision: u64,
    data: String,
}
type Update = Option<Result<Arc<Frame>, ()>>;
type Sample = std::pin::Pin<
    Box<
        dyn std::future::Future<
                Output = Result<
                    crate::plugin_management::status::Sample,
                    crate::plugin_management::ManagementError,
                >,
            > + Send,
    >,
>;

pub(super) struct Worker {
    manager: PluginManager,
    observer: RuntimeObserver,
    updates: watch::Sender<Update>,
    shutdown: Option<ShutdownSignal>,
    stop: oneshot::Receiver<()>,
}
impl Worker {
    pub(super) async fn run(mut self) {
        let mut interval = tokio::time::interval(COALESCE);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut plugins = Value::Null;
        let mut next_sample = tokio::time::Instant::now();
        let mut hint = 0;
        let mut failed = false;
        let mut payload = String::new();
        let mut revision = 0_u64;
        let mut sample: Option<Sample> = None;
        loop {
            tokio::select! {
                biased;
                _ = &mut self.stop => break,
                () = cancelled(&mut self.shutdown) => break,
                result = async { match &mut sample {
                    Some(sample) => sample.as_mut().await,
                    None => std::future::pending().await,
                }} => {
                sample = None;
                match result {
                    Ok((rows, truncated, bundled_revision, data_revisions)) => {
                        plugins = json!({"plugins": rows, "plugins_truncated": truncated, "bundled_revision": bundled_revision, "plugin_data_revisions":data_revisions});
                        failed = false;
                    }
                    Err(_) => {
                        failed = true;
                    }
                }
                next_sample = tokio::time::Instant::now() + SAMPLE;
                }
                _ = interval.tick() => {}
            }
            if sample.is_none()
                && (tokio::time::Instant::now() >= next_sample
                    || hint != self.manager.status_revision())
            {
                hint = self.manager.status_revision();
                let manager = self.manager.clone();
                sample = Some(Box::pin(async move { manager.status().await }) as Sample);
            }
            if failed {
                self.updates.send_if_modified(|current| {
                    if matches!(current, Some(Err(()))) {
                        false
                    } else {
                        *current = Some(Err(()));
                        true
                    }
                });
                continue;
            }
            if plugins.is_null() {
                continue;
            }
            let mut next = plugins.clone();
            next["boot_id"] = json!(crate::restart::boot_id());
            next["runtime"] = json!(super::runtime::RuntimeStatus::read(&self.observer));
            // No reliable startup-vs-saved non-hot settings fingerprint exists yet.
            next["settings_restart_required"] = Value::Null;
            let encoded = next.to_string();
            if encoded != payload || matches!(*self.updates.borrow(), Some(Err(()))) {
                let Some(incremented) = revision.checked_add(1) else {
                    break;
                };
                revision = incremented;
                payload = encoded;
                next["revision"] = json!(revision.to_string());
                self.updates.send_replace(Some(Ok(Arc::new(Frame {
                    revision,
                    data: next.to_string(),
                }))));
            }
        }
        if let Some(sample) = sample {
            let _ = sample.await;
        }
    }
}
#[derive(Clone)]
struct Context {
    updates: watch::Receiver<Update>,
    auth: AuthState,
    shutdown: Option<ShutdownSignal>,
}
pub(super) fn router(
    manager: PluginManager,
    observer: RuntimeObserver,
    auth: AuthState,
    shutdown: Option<ShutdownSignal>,
) -> (Router, Worker, oneshot::Sender<()>) {
    let (updates, receiver) = watch::channel(None);
    let (stop, stopped) = oneshot::channel();
    let worker = Worker {
        manager,
        observer,
        updates,
        shutdown: shutdown.clone(),
        stop: stopped,
    };
    let router = Router::new()
        .route("/api/status/stream", get(stream))
        .with_state(Context {
            updates: receiver,
            auth: auth.clone(),
            shutdown,
        })
        .route_layer(middleware::from_fn(super::plugins::read_body))
        .route_layer(middleware::from_fn_with_state(
            Arc::new(Semaphore::new(MAX_STREAMS)),
            reserve,
        ))
        .route_layer(middleware::from_fn_with_state(auth, auth::authenticate));
    (router, worker, stop)
}
struct Stream {
    context: Context,
    headers: HeaderMap,
    _permit: Arc<OwnedSemaphorePermit>,
    revision: Option<u64>,
    stale: bool,
    finished: bool,
}
async fn reserve(
    State(slots): State<Arc<Semaphore>>,
    mut request: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    let Ok(permit) = slots.try_acquire_owned() else {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            axum::Json(json!({"error":"状态订阅已达上限"})),
        )
            .into_response();
    };
    request.extensions_mut().insert(Arc::new(permit));
    next.run(request).await
}
async fn stream(
    State(context): State<Context>,
    Extension(permit): Extension<Arc<OwnedSemaphorePermit>>,
    headers: HeaderMap,
) -> Response {
    // Every connection resumes with the newest FULL snapshot, regardless of Last-Event-ID.
    let state = Stream {
        context,
        headers,
        _permit: permit,
        revision: None,
        stale: false,
        finished: false,
    };
    let events = futures_util::stream::unfold(state, |mut state| async move {
        let event = state.next().await?;
        Some((Ok::<_, Infallible>(event), state))
    });
    let mut response = Sse::new(events)
        .keep_alive(KeepAlive::new().interval(HEARTBEAT))
        .into_response();
    response
        .headers_mut()
        .insert("x-accel-buffering", "no".parse().expect("static header"));
    response
}
impl Stream {
    async fn next(&mut self) -> Option<Event> {
        let mut changes = self.context.auth.changes();
        loop {
            if self.finished
                || self
                    .context
                    .shutdown
                    .as_ref()
                    .is_some_and(ShutdownSignal::is_shutdown)
            {
                return None;
            }
            if !self.context.auth.authenticated(&self.headers) {
                self.finished = true;
                return Some(Event::default().event("auth-required").data("{}"));
            }
            let sample = self.context.updates.borrow_and_update().clone();
            match sample {
                Some(Ok(frame)) if self.revision != Some(frame.revision) || self.stale => {
                    self.revision = Some(frame.revision);
                    self.stale = false;
                    return Some(
                        Event::default()
                            .event("status")
                            .id(format!("{}:{}", crate::restart::boot_id(), frame.revision))
                            .data(frame.data.clone()),
                    );
                }
                Some(Err(())) if !self.stale => {
                    self.stale = true;
                    return Some(Event::default().event("stale").data("{}"));
                }
                _ => {}
            }
            let delay = self
                .context
                .auth
                .expires_at(&self.headers)
                .map_or(HEARTBEAT, |expires| {
                    expires
                        .saturating_duration_since(std::time::Instant::now())
                        .min(HEARTBEAT)
                });
            tokio::select! {
                biased;
                () = cancelled(&mut self.context.shutdown) => return None,
                result = changes.changed() => { if result.is_err() { return None; } }
                result = self.context.updates.changed() => { if result.is_err() { return None; } }
                () = tokio::time::sleep(delay) => {}
            }
        }
    }
}
async fn cancelled(shutdown: &mut Option<ShutdownSignal>) {
    if let Some(shutdown) = shutdown {
        shutdown.cancelled().await;
    } else {
        std::future::pending::<()>().await;
    }
}

#[cfg(test)]
#[path = "status_stream_tests.rs"]
mod tests;
