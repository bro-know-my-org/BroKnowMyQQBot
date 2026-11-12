//! Deterministic lifecycle interleavings: explicitly poll each waiter at its gate.

use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::Notify;

struct Probe {
    manifest: PluginManifest,
    starts: AtomicUsize,
    stops: AtomicUsize,
    schedules: AtomicUsize,
    health: AtomicUsize,
    block_init: AtomicBool,
    block_validation: AtomicBool,
    block_shutdown: AtomicBool,
    release: Notify,
    fired: Notify,
}

#[async_trait]
impl StaticPlugin for Probe {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    async fn validate_config(&self, _: &BTreeMap<String, Value>) -> Result<(), PluginError> {
        if self.block_validation.load(Ordering::SeqCst) {
            self.release.notified().await;
        }
        Ok(())
    }
    async fn init(&self, _: InitContext) -> Result<(), PluginError> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        if self.block_init.load(Ordering::SeqCst) {
            self.release.notified().await;
        }
        Ok(())
    }
    async fn shutdown(&self) -> Result<(), PluginError> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        if self.block_shutdown.load(Ordering::SeqCst) {
            self.release.notified().await;
        }
        Ok(())
    }
    async fn health(&self) -> Result<HealthStatus, PluginError> {
        match self.health.load(Ordering::SeqCst) {
            0 => Ok(HealthStatus::Healthy),
            1 => Ok(HealthStatus::Degraded),
            2 => Err(PluginError::GuestTrap("poisoned health".to_owned())),
            _ => Err(PluginError::ResourceExhaustedTrap(
                "poisoned health".to_owned(),
            )),
        }
    }
    async fn on_event(
        &self,
        event: &PluginEventEnvelope,
        _: &dyn HostQueries,
    ) -> Result<HandlerOutput, PluginError> {
        if event.event_type == "recovery.create" {
            return Ok(HandlerOutput {
                commands: vec![PluginCommand {
                    command_id: "create".to_owned(),
                    kind: "schedule.create".to_owned(),
                    idempotency_key: None,
                    deadline_ms: None,
                    payload: serde_json::json!({
                        "task_id": "replayed-immediate", "run_at_ms": 0, "payload": null,
                    }),
                }],
                ..HandlerOutput::default()
            });
        }
        if event.event_type == "recovery.wait" {
            self.release.notified().await;
            return Ok(HandlerOutput::default());
        }
        if event.event_type == "schedule.triggered" {
            self.schedules.fetch_add(1, Ordering::SeqCst);
            self.fired.notify_one();
            Ok(HandlerOutput::default())
        } else {
            self.release.notified().await;
            Err(PluginError::GuestTrap(
                "active invocation poisoned guest".to_owned(),
            ))
        }
    }
}

fn probe() -> Arc<Probe> {
    let mut manifest = builtin_plugins::PingPlugin::default().manifest().clone();
    manifest.subscriptions[0].event = "schedule.triggered".to_owned();
    manifest.subscriptions[0].scopes.clear();
    manifest.permissions.scheduler = true;
    manifest.permissions.storage_quota_bytes = 100_000;
    Arc::new(Probe {
        manifest,
        starts: AtomicUsize::new(0),
        stops: AtomicUsize::new(0),
        schedules: AtomicUsize::new(0),
        health: AtomicUsize::new(0),
        block_init: AtomicBool::new(false),
        block_validation: AtomicBool::new(false),
        block_shutdown: AtomicBool::new(false),
        release: Notify::new(),
        fired: Notify::new(),
    })
}

async fn fixture() -> (StaticPluginHost, Arc<Probe>) {
    let probe = probe();
    let mut host = StaticPluginHost::new(PluginStore::in_memory().unwrap());
    host.register_trusted(probe.clone(), "fixture", BTreeMap::new())
        .await
        .unwrap();
    (host, probe)
}

#[tokio::test]
async fn poisoning_while_stop_apply_upgrade_or_shutdown_waits_never_revives_the_guest() {
    for operation in 0..4 {
        let (host, probe) = fixture().await;
        let mut upgrading_host = host.clone();
        let plugin = &host.plugins[0];
        let invocation = async {
            let _execution = plugin.execution.acquire("active".to_owned()).await.unwrap();
            let envelope = PluginEventEnvelope {
                protocol_version: BPP_VERSION.to_owned(),
                event_id: "poison".to_owned(),
                delivery_id: "delivery".to_owned(),
                invocation_id: "invocation".to_owned(),
                occurred_at_ms: None,
                received_at_ms: now_ms(),
                adapter_id: "test".to_owned(),
                event_type: "message".to_owned(),
                trace_id: None,
                payload: Value::Null,
                extensions: Vec::new(),
            };
            host.deliver_with_retry(
                plugin,
                ExecutionOrigin::Scheduled { adapter_id: "test" },
                envelope,
            )
            .await
        };
        tokio::pin!(invocation);
        assert!(futures_util::poll!(&mut invocation).is_pending());
        let management = async {
            match operation {
                0 => host.set_instance_enabled("fixture", false).await,
                1 => {
                    host.update_config(
                        "fixture",
                        BTreeMap::from([("new".to_owned(), Value::Bool(true))]),
                    )
                    .await
                }
                2 => host.shutdown().await,
                _ => {
                    upgrading_host
                        .upgrade_trusted("fixture", probe.clone(), BTreeMap::new())
                        .await
                }
            }
        };
        tokio::pin!(management);
        assert!(futures_util::poll!(&mut management).is_pending());
        assert_eq!(plugin.state().unwrap(), PluginInstanceState::Draining);
        probe.release.notify_one();
        invocation.await.unwrap();
        let result = management.await;
        assert_eq!(result.is_err(), operation == 1 || operation == 3);
        assert_eq!(plugin.state().unwrap(), PluginInstanceState::Disabled);
        assert!(host.set_instance_enabled("fixture", true).await.is_err());
        assert!(
            host.update_config("fixture", BTreeMap::new())
                .await
                .is_err()
        );
        assert!(
            host.configure_stopped_instance("fixture", BTreeMap::new())
                .await
                .is_err()
        );
        assert_eq!(plugin.config_snapshot().unwrap(), BTreeMap::new());
        assert_eq!(probe.starts.load(Ordering::SeqCst), 1);
        assert_eq!(probe.stops.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn simultaneous_enable_initializes_once_and_serializes_stopped_configuration() {
    let (host, probe) = fixture().await;
    host.set_instance_enabled("fixture", false).await.unwrap();
    probe.block_init.store(true, Ordering::SeqCst);
    let first = host.set_instance_enabled("fixture", true);
    let clone = host.clone();
    let second = clone.set_instance_enabled("fixture", true);
    let configure = clone.configure_stopped_instance("fixture", BTreeMap::new());
    tokio::pin!(first, second, configure);
    assert!(futures_util::poll!(&mut first).is_pending());
    assert!(futures_util::poll!(&mut second).is_pending());
    assert!(futures_util::poll!(&mut configure).is_pending());
    probe.release.notify_one();
    first.await.unwrap();
    second.await.unwrap();
    assert!(configure.await.is_err());
    assert_eq!(probe.starts.load(Ordering::SeqCst), 2);
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn rejected_resume_cleans_up_degraded_health_but_never_calls_poisoned_guests() {
    for health in 1..=3 {
        let (host, probe) = fixture().await;
        host.set_instance_enabled("fixture", false).await.unwrap();
        probe.health.store(health, Ordering::SeqCst);
        assert!(host.set_instance_enabled("fixture", true).await.is_err());
        assert_eq!(
            host.instance_state("fixture").unwrap(),
            Some(PluginInstanceState::Disabled)
        );
        assert!(host.set_instance_enabled("fixture", true).await.is_err());
        host.shutdown().await.unwrap();
        assert_eq!(probe.starts.load(Ordering::SeqCst), 2);
        assert_eq!(
            probe.stops.load(Ordering::SeqCst),
            if health == 1 { 2 } else { 1 }
        );
    }
}

#[tokio::test]
async fn due_and_queued_schedules_survive_stop_and_resume_without_requeuing_other_tasks() {
    let (host, probe) = fixture().await;
    let plugin = &host.plugins[0];
    let task = |id: &str| ScheduledTask {
        instance_id: "fixture".to_owned(),
        task_id: id.to_owned(),
        adapter_id: "test".to_owned(),
        run_at_ms: 0,
        payload: Value::Null,
    };
    let queued_task = task("queued");
    let due_task = task("due");
    let other_task = task("other");
    for task in [&queued_task, &due_task, &other_task] {
        host.store.create_schedule(task, 100_000).unwrap();
    }
    // Another task is already firing; refusing this invocation must not reset it.
    assert!(host.store.claim_schedule("fixture", "other").unwrap());
    let active = plugin.execution.acquire("active".to_owned()).await.unwrap();
    let queued = host.run_schedule(plugin, &queued_task);
    tokio::pin!(queued);
    assert!(futures_util::poll!(&mut queued).is_pending());
    let stop = host.set_instance_enabled("fixture", false);
    tokio::pin!(stop);
    assert!(futures_util::poll!(&mut stop).is_pending());
    // Due after Draining: no delivery may start, and its row must remain pending.
    host.run_schedule(plugin, &due_task).await.unwrap();
    assert!(!host.store.claim_schedule("fixture", "other").unwrap());
    host.store
        .finish_schedule("fixture", "other", "completed")
        .unwrap();
    drop(active);
    queued.await.unwrap();
    stop.await.unwrap();
    host.set_instance_enabled("fixture", true).await.unwrap();
    timeout(Duration::from_secs(2), async {
        while probe.schedules.load(Ordering::SeqCst) < 2 {
            probe.fired.notified().await;
        }
    })
    .await
    .unwrap();
    host.shutdown().await.unwrap();
    assert!(host.store.recover_schedules("fixture").unwrap().is_empty());
    assert_eq!(probe.schedules.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn cancelling_shutdown_preserves_schedules_for_instances_not_yet_drained() {
    for wait_for_lifecycle in [true, false] {
        let (mut host, first) = fixture().await;
        let mut second = probe();
        Arc::get_mut(&mut second).unwrap().manifest.commands.clear();
        host.register_trusted(second.clone(), "second", BTreeMap::new())
            .await
            .unwrap();
        for plugin in &host.plugins {
            let task = ScheduledTask {
                instance_id: plugin.instance_id.clone(),
                task_id: "shutdown-cancel".to_owned(),
                adapter_id: "test".to_owned(),
                run_at_ms: 0,
                payload: Value::Null,
            };
            host.store.create_schedule(&task, 100_000).unwrap();
            host.launch_schedule(plugin, &task).unwrap();
        }
        let plugin = &host.plugins[0];
        let lifecycle = if wait_for_lifecycle {
            Some(plugin.lifecycle.lock().await)
        } else {
            None
        };
        let execution = plugin.execution.acquire("active".to_owned()).await.unwrap();
        {
            let shutdown = host.shutdown();
            tokio::pin!(shutdown);
            assert!(futures_util::poll!(&mut shutdown).is_pending());
        }
        assert_eq!(plugin.state().unwrap(), PluginInstanceState::Ready);
        assert!(
            host.schedule_handles()
                .unwrap()
                .contains_key(&("second".to_owned(), "shutdown-cancel".to_owned()))
        );
        drop(execution);
        drop(lifecycle);
        for probe in [&first, &second] {
            timeout(Duration::from_secs(2), probe.fired.notified())
                .await
                .unwrap();
            assert_eq!(probe.schedules.load(Ordering::SeqCst), 1);
        }
        host.shutdown().await.unwrap();
        assert!(host.schedule_handles().unwrap().is_empty());
    }
}
