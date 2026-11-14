use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use async_trait::async_trait;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use bot_core::{
    Action, ActionResult, Adapter, AdapterError, AdapterId, Event, EventEnvelope, EventId,
    EventSender, RuntimeBuilder, RuntimeObserver, ShutdownSignal, shutdown_channel,
};
use plugin_host::{PluginStore, StaticPluginHost};
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc, oneshot};
use tower::ServiceExt as _;

use crate::plugin_management::PluginManager;

const TOKEN: &str = "runtime-test-token-0123456789abcdef0123456789";

fn app(observer: RuntimeObserver) -> Router {
    let store = PluginStore::in_memory().unwrap();
    let manager = PluginManager::new(
        StaticPluginHost::new(store.clone()),
        store,
        BTreeMap::default(),
        BTreeSet::default(),
    );
    let auth = super::super::plugins::token_digest(TOKEN).unwrap().into();
    let (_, shutdown) = shutdown_channel();
    super::super::router_parts(
        observer,
        Some((
            manager,
            auth,
            crate::logging::live::LogBuffer::default(),
            shutdown,
        )),
    )
    .0
}

async fn read(app: &Router, path: &str, token: Option<&str>) -> (StatusCode, Value) {
    let mut request = Request::get(path);
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(
        response.headers()[header::X_CONTENT_TYPE_OPTIONS],
        "nosniff"
    );
    let status = response.status();
    let body = to_bytes(response.into_body(), 256 * 1024).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

fn event(adapter: &str) -> EventEnvelope {
    EventEnvelope {
        id: EventId::new("fixture"),
        adapter: AdapterId::new(adapter),
        delivery_id: None,
        timestamp: None,
        event: Event::Lifecycle(Value::Null),
        raw: json!({"secret":"not-for-management"}),
    }
}

#[tokio::test]
async fn runtime_authentication_safe_counters_and_bounded_response() {
    let observer = RuntimeObserver::new();
    let app = app(observer.clone());
    for token in [None, Some("wrong")] {
        assert_eq!(
            read(&app, "/api/runtime", token).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    let (tx, _rx) = mpsc::channel(1);
    for index in 0..300 {
        let id = format!("adapter-{index}");
        let sender = EventSender::new(tx.clone(), AdapterId::new(&id), observer.clone()).unwrap();
        sender.mark_ready();
        let _ = sender.try_send(event(&id));
    }
    let (status, data) = read(&app, "/api/runtime", Some(TOKEN)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data["snapshot"]["adapters"].as_object().unwrap().len(), 256);
    assert_eq!(data["snapshot"]["events_received"], "1");
    assert_eq!(data["snapshot"]["rejected_events"], "299");
    assert_eq!(data["snapshot"]["queue_depth"], 1);
    assert!(!data.to_string().contains("not-for-management"));
    assert!(data["snapshot"].get("handlers").is_none());
}

struct FixtureAdapter {
    id: AdapterId,
    commands: Mutex<mpsc::Receiver<(bool, oneshot::Sender<()>)>>,
    fail: bool,
}

#[async_trait]
impl Adapter for FixtureAdapter {
    fn id(&self) -> &AdapterId {
        &self.id
    }
    fn platform(&self) -> &'static str {
        "fixture"
    }
    async fn run(
        &self,
        events: EventSender,
        mut shutdown: ShutdownSignal,
    ) -> Result<(), AdapterError> {
        let mut commands = self.commands.lock().await;
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                command = commands.recv() => {
                    let Some((online, done)) = command else { break };
                    if online { events.mark_ready(); } else { events.mark_not_ready(); }
                    let _ = done.send(());
                }
            }
        }
        if self.fail {
            Err(AdapterError::Transport("credential=private-fixture".into()))
        } else {
            Ok(())
        }
    }
    async fn execute(&self, _: Action) -> Result<ActionResult, AdapterError> {
        unreachable!("fixture sends no actions")
    }
}

#[tokio::test]
async fn runtime_endpoint_tracks_real_readiness_phase_and_safe_transport_failures() {
    for fail in [false, true] {
        let observer = RuntimeObserver::new();
        let app = app(observer.clone());
        assert_eq!(
            read(&app, "/api/runtime", Some(TOKEN)).await.1["snapshot"]["phase"],
            "stopped"
        );
        let (commands, receiver) = mpsc::channel(1);
        let adapter = Arc::new(FixtureAdapter {
            id: AdapterId::new("fixture"),
            commands: Mutex::new(receiver),
            fail,
        });
        let (handle, signal) = shutdown_channel();
        let runtime = RuntimeBuilder::new()
            .adapter(adapter)
            .queue_capacity(8)
            .observer(observer)
            .build()
            .unwrap();
        let task = tokio::spawn(runtime.run(signal));
        for online in [false, true, false] {
            let (done, acknowledged) = oneshot::channel();
            commands.send((online, done)).await.unwrap();
            acknowledged.await.unwrap();
            let (status, data) = read(&app, "/api/runtime", Some(TOKEN)).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(data["health"]["configured_adapters"], 1);
            assert_eq!(data["health"]["online_adapters"], usize::from(online));
            assert_eq!(
                data["health"]["status"],
                if online { "healthy" } else { "degraded" }
            );
            assert_eq!(data["snapshot"]["phase"], "running");
            assert_eq!(data["snapshot"]["queue_capacity"], 8);
            assert_eq!(data["snapshot"]["adapters"]["fixture"]["online"], online);
            assert_eq!(
                read(&app, "/healthz", None).await.0,
                if online {
                    StatusCode::OK
                } else {
                    StatusCode::SERVICE_UNAVAILABLE
                }
            );
        }
        if fail {
            drop(commands);
        } else {
            handle.shutdown();
        }
        assert_eq!(task.await.unwrap().is_err(), fail);
        let (status, data) = read(&app, "/api/runtime", Some(TOKEN)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            data["snapshot"]["phase"],
            if fail { "failed" } else { "stopped" }
        );
        let adapter = &data["snapshot"]["adapters"]["fixture"];
        assert_eq!(adapter["online"], false);
        assert_eq!(adapter["failures"], if fail { "1" } else { "0" });
        if fail {
            assert_eq!(adapter["last_failure"], "transport");
        }
        assert!(!data.to_string().contains("private-fixture"));
    }
}
