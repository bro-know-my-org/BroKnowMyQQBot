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
async fn unregister_healthy_or_stopped_guest_is_irreversible_in_old_host_clones() {
    for stopped in [false, true] {
        let (mut host, probe) = fixture().await;
        let old = host.clone();
        if stopped {
            host.set_instance_enabled("fixture", false).await.unwrap();
        }
        host.unregister_instance("fixture").await.unwrap();
        assert!(host.instance_snapshots().unwrap().is_empty());
        assert!(old.set_instance_enabled("fixture", true).await.is_err());
        assert_eq!(
            old.instance_snapshots().unwrap()[0].state,
            PluginInstanceState::Disabled
        );
        assert_eq!(probe.starts.load(Ordering::SeqCst), 1);
        assert_eq!(probe.stops.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn poisoning_while_stop_apply_upgrade_or_shutdown_waits_never_revives_the_guest() {
    for operation in 0..5 {
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
                3 => {
                    upgrading_host
                        .upgrade_trusted("fixture", probe.clone(), BTreeMap::new())
                        .await
                }
                _ => upgrading_host.unregister_instance("fixture").await,
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

#[tokio::test]
async fn cancelling_management_while_waiting_for_execution_allows_shutdown() {
    for operation in 0..4 {
        let (host, probe) = fixture().await;
        let mut upgrading_host = host.clone();
        let plugin = &host.plugins[0];
        let active = plugin.execution.acquire("active".to_owned()).await.unwrap();
        {
            let management = async {
                match operation {
                    0 => host.set_instance_enabled("fixture", false).await,
                    1 => host.update_config("fixture", BTreeMap::new()).await,
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
        }
        assert_eq!(plugin.state().unwrap(), PluginInstanceState::Ready);
        drop(active);
        host.shutdown().await.unwrap();
        assert_eq!(plugin.state().unwrap(), PluginInstanceState::Stopped);
        assert_eq!(probe.stops.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn cancelling_resume_during_init_permanently_disables_the_guest() {
    let (host, probe) = fixture().await;
    host.set_instance_enabled("fixture", false).await.unwrap();
    probe.block_init.store(true, Ordering::SeqCst);
    {
        let resume = host.set_instance_enabled("fixture", true);
        tokio::pin!(resume);
        assert!(futures_util::poll!(&mut resume).is_pending());
        assert_eq!(probe.starts.load(Ordering::SeqCst), 2);
    }
    assert_eq!(
        host.plugins[0].state().unwrap(),
        PluginInstanceState::Disabled
    );
    assert!(host.set_instance_enabled("fixture", true).await.is_err());
    host.shutdown().await.unwrap();
    assert_eq!(probe.stops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancelling_configuration_or_upgrade_hooks_never_reuses_the_guest() {
    for operation in 0..4 {
        let (host, probe) = fixture().await;
        let mut upgrading_host = host.clone();
        if operation == 0 {
            host.set_instance_enabled("fixture", false).await.unwrap();
        }
        if operation < 2 {
            probe.block_validation.store(true, Ordering::SeqCst);
        } else {
            probe.block_init.store(true, Ordering::SeqCst);
        }
        {
            let management = async {
                match operation {
                    0 => {
                        host.configure_stopped_instance("fixture", BTreeMap::new())
                            .await
                    }
                    1 | 2 => host.update_config("fixture", BTreeMap::new()).await,
                    _ => {
                        upgrading_host
                            .upgrade_trusted("fixture", probe.clone(), BTreeMap::new())
                            .await
                    }
                }
            };
            tokio::pin!(management);
            assert!(futures_util::poll!(&mut management).is_pending());
        }
        assert_eq!(
            host.plugins[0].state().unwrap(),
            PluginInstanceState::Disabled
        );
        assert!(host.set_instance_enabled("fixture", true).await.is_err());
        let stops = probe.stops.load(Ordering::SeqCst);
        host.shutdown().await.unwrap();
        assert_eq!(probe.stops.load(Ordering::SeqCst), stops);
        assert_eq!(host.plugins[0].config_snapshot().unwrap(), BTreeMap::new());
    }
}

#[tokio::test]
async fn cancelling_shutdown_hook_disables_instead_of_retrying_the_hook() {
    let (host, probe) = fixture().await;
    probe.block_shutdown.store(true, Ordering::SeqCst);
    {
        let stop = host.set_instance_enabled("fixture", false);
        tokio::pin!(stop);
        assert!(futures_util::poll!(&mut stop).is_pending());
    }
    assert_eq!(
        host.plugins[0].state().unwrap(),
        PluginInstanceState::Disabled
    );
    host.shutdown().await.unwrap();
    assert_eq!(probe.stops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn resume_recovery_errors_do_not_leave_a_successful_ready_instance() {
    for table in ["command_ledger", "plugin_deliveries", "scheduled_tasks"] {
        let (mut host, probe) = fixture().await;
        let uri = format!(
            "file:management-{}?mode=memory&cache=shared",
            Uuid::new_v4()
        );
        host.store = PluginStore::open(&uri).unwrap();
        host.set_instance_enabled("fixture", false).await.unwrap();
        let connection = rusqlite::Connection::open(&uri).unwrap();
        connection
            .execute_batch(&format!("DROP TABLE {table}"))
            .unwrap();
        assert!(host.set_instance_enabled("fixture", true).await.is_err());
        assert_eq!(
            host.plugins[0].state().unwrap(),
            PluginInstanceState::Disabled
        );
        assert!(host.set_instance_enabled("fixture", true).await.is_err());
        assert_eq!(probe.starts.load(Ordering::SeqCst), 2);
    }
}

#[tokio::test]
async fn resume_recovery_that_poisons_the_guest_cannot_report_success() {
    let (host, probe) = fixture().await;
    host.set_instance_enabled("fixture", false).await.unwrap();
    let envelope = PluginEventEnvelope {
        protocol_version: BPP_VERSION.to_owned(),
        event_id: "recovered".to_owned(),
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
    host.store
        .enqueue_delivery(
            "fixture",
            &envelope,
            &ExecutionOrigin::Scheduled { adapter_id: "test" }.outbox_origin("recovered"),
            now_ms(),
        )
        .unwrap();
    let resume = host.set_instance_enabled("fixture", true);
    tokio::pin!(resume);
    assert!(futures_util::poll!(&mut resume).is_pending());
    // Recovery may invoke the guest, but ordinary dispatch must stay gated.
    assert_eq!(
        host.plugins[0].state().unwrap(),
        PluginInstanceState::Draining
    );
    probe.release.notify_one();
    assert!(resume.await.is_err());
    assert_eq!(
        host.plugins[0].state().unwrap(),
        PluginInstanceState::Disabled
    );
    assert!(host.set_instance_enabled("fixture", true).await.is_err());
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelled_stop_restarts_due_work_that_requeued_during_draining() {
    let (host, probe) = fixture().await;
    let plugin = &host.plugins[0];
    let task = ScheduledTask {
        instance_id: "fixture".to_owned(),
        task_id: "due".to_owned(),
        adapter_id: "test".to_owned(),
        run_at_ms: 0,
        payload: Value::Null,
    };
    host.store.create_schedule(&task, 100_000).unwrap();
    let active = plugin.execution.acquire("active".to_owned()).await.unwrap();
    {
        let stop = host.set_instance_enabled("fixture", false);
        tokio::pin!(stop);
        assert!(futures_util::poll!(&mut stop).is_pending());
        host.run_schedule(plugin, &task).await.unwrap();
    }
    assert_eq!(plugin.state().unwrap(), PluginInstanceState::Ready);
    assert!(
        host.scheduled_tasks
            .lock()
            .unwrap()
            .contains_key(&("fixture".to_owned(), "due".to_owned()))
    );
    drop(active);
    timeout(Duration::from_secs(2), probe.fired.notified())
        .await
        .unwrap();
    host.shutdown().await.unwrap();
    assert_eq!(probe.starts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn recovered_immediate_schedule_waits_for_ready_before_starting_a_worker() {
    let (host, probe) = fixture().await;
    host.set_instance_enabled("fixture", false).await.unwrap();
    for (id, event_type) in [("a-create", "recovery.create"), ("b-wait", "recovery.wait")] {
        let envelope = PluginEventEnvelope {
            protocol_version: BPP_VERSION.to_owned(),
            event_id: id.to_owned(),
            delivery_id: "delivery".to_owned(),
            invocation_id: "invocation".to_owned(),
            occurred_at_ms: None,
            received_at_ms: now_ms(),
            adapter_id: "test".to_owned(),
            event_type: event_type.to_owned(),
            trace_id: None,
            payload: Value::Null,
            extensions: Vec::new(),
        };
        host.store
            .enqueue_delivery(
                "fixture",
                &envelope,
                &ExecutionOrigin::Scheduled { adapter_id: "test" }.outbox_origin(id),
                0,
            )
            .unwrap();
    }
    let resume = host.set_instance_enabled("fixture", true);
    tokio::pin!(resume);
    assert!(futures_util::poll!(&mut resume).is_pending());
    assert_eq!(
        host.plugins[0].state().unwrap(),
        PluginInstanceState::Draining
    );
    assert!(
        host.store
            .pending_schedule("fixture", "replayed-immediate")
            .unwrap()
            .is_some()
    );
    assert!(host.scheduled_tasks.lock().unwrap().is_empty());
    probe.release.notify_one();
    resume.await.unwrap();
    timeout(Duration::from_secs(2), probe.fired.notified())
        .await
        .unwrap();
    host.shutdown().await.unwrap();
    assert_eq!(probe.schedules.load(Ordering::SeqCst), 1);
    assert!(host.store.recover_schedules("fixture").unwrap().is_empty());
}

#[tokio::test]
async fn schedule_storage_failures_retire_without_respawning_and_allow_explicit_recovery() {
    for fail_claim in [true, false] {
        let (mut host, probe) = fixture().await;
        let uri = format!(
            "file:schedule-failure-{}?mode=memory&cache=shared",
            Uuid::new_v4()
        );
        host.store = PluginStore::open(&uri).unwrap();
        let connection = rusqlite::Connection::open(&uri).unwrap();
        let task = ScheduledTask {
            instance_id: "fixture".to_owned(),
            task_id: "storage-failure".to_owned(),
            adapter_id: "test".to_owned(),
            run_at_ms: 0,
            payload: Value::Null,
        };
        host.store.create_schedule(&task, 100_000).unwrap();
        // Exhaust writes deterministically while leaving pending-row reads usable.
        let fault_target = if fail_claim {
            "UPDATE OF status ON scheduled_tasks WHEN NEW.status = 'firing'"
        } else {
            "INSERT ON plugin_deliveries"
        };
        connection
            .execute_batch(&format!(
                "CREATE TABLE fault_pages (payload BLOB);
                 CREATE TRIGGER fail_schedule_write BEFORE {fault_target}
                 BEGIN INSERT INTO fault_pages VALUES (zeroblob(1048576)); END;"
            ))
            .unwrap();
        let pages: u32 = connection
            .query_row("PRAGMA page_count", [], |row| row.get(0))
            .unwrap();
        connection
            .pragma_update(None, "max_page_count", pages)
            .unwrap();
        if fail_claim {
            assert!(matches!(
                host.store.claim_schedule("fixture", &task.task_id),
                Err(StoreError::Sqlite(rusqlite::Error::SqliteFailure(error, _)))
                    if error.code == rusqlite::ErrorCode::DiskFull
            ));
        }
        let plugin = &host.plugins[0];
        host.launch_schedule(plugin, &task).unwrap();
        let worker = host
            .schedule_handles()
            .unwrap()
            .values()
            .next()
            .unwrap()
            .clone();
        timeout(Duration::from_secs(2), async {
            while !worker.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            host.schedule_handles().unwrap().is_empty(),
            "failed worker respawned"
        );
        let status: String = connection
            .query_row(
                "SELECT status FROM scheduled_tasks WHERE instance_id = 'fixture' AND task_id = ?",
                [&task.task_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, if fail_claim { "pending" } else { "firing" });
        assert_eq!(probe.schedules.load(Ordering::SeqCst), 0);

        connection
            .execute_batch("DROP TRIGGER fail_schedule_write")
            .unwrap();
        host.store.recover_schedules("fixture").unwrap();
        host.resume_pending_schedules(plugin).unwrap();
        timeout(Duration::from_secs(2), probe.fired.notified())
            .await
            .unwrap();
        host.shutdown().await.unwrap();
        assert_eq!(probe.schedules.load(Ordering::SeqCst), 1);
        assert!(host.store.recover_schedules("fixture").unwrap().is_empty());
    }
}

#[tokio::test]
async fn cancelled_management_rearms_workers_retiring_before_or_after_rollback() {
    for operation in 0..3 {
        for retire_due_first in [true, false] {
            let (host, probe) = fixture().await;
            let mut upgrading_host = host.clone();
            let plugin = &host.plugins[0];
            let generation = plugin.ready_generation().unwrap().unwrap();
            let task = |id: &str| ScheduledTask {
                instance_id: "fixture".to_owned(),
                task_id: id.to_owned(),
                adapter_id: "test".to_owned(),
                run_at_ms: 0,
                payload: Value::Null,
            };
            let queued_task = task("queued-late");
            let due_task = task("due-retiring");
            for task in [&queued_task, &due_task] {
                host.store.create_schedule(task, 100_000).unwrap();
            }
            // Drive the real worker body/retirement separately. These inert
            // tasks reserve their worker IDs so each interleaving is explicit.
            let queued_slot = tokio::spawn(std::future::pending::<()>());
            let due_slot = tokio::spawn(std::future::pending::<()>());
            {
                let mut handles = host.scheduled_tasks.lock().unwrap();
                handles.insert(
                    ("fixture".to_owned(), queued_task.task_id.clone()),
                    queued_slot.abort_handle(),
                );
                handles.insert(
                    ("fixture".to_owned(), due_task.task_id.clone()),
                    due_slot.abort_handle(),
                );
            }
            let active = plugin.execution.acquire("active".to_owned()).await.unwrap();
            let queued = host.run_schedule_owned(plugin, &queued_task, Some(queued_slot.id()));
            tokio::pin!(queued);
            assert!(futures_util::poll!(&mut queued).is_pending());
            {
                let management = async {
                    match operation {
                        0 => host.set_instance_enabled("fixture", false).await,
                        1 => host.update_config("fixture", BTreeMap::new()).await,
                        _ => {
                            upgrading_host
                                .upgrade_trusted("fixture", probe.clone(), BTreeMap::new())
                                .await
                        }
                    }
                };
                tokio::pin!(management);
                assert!(futures_util::poll!(&mut management).is_pending());
                host.run_schedule_owned(plugin, &due_task, Some(due_slot.id()))
                    .await
                    .unwrap();
                if retire_due_first {
                    host.retire_schedule(plugin, &due_task, due_slot.id(), true)
                        .unwrap();
                }
            }
            assert_eq!(plugin.state().unwrap(), PluginInstanceState::Ready);
            assert!(!plugin.is_ready_generation(generation).unwrap());
            // No lifecycle hook ran. In particular, the rollback sweep must
            // leave the old queued worker's firing row/claim alone.
            assert_eq!(probe.starts.load(Ordering::SeqCst), 1);
            assert_eq!(probe.stops.load(Ordering::SeqCst), 0);
            assert!(
                !host
                    .store
                    .claim_schedule("fixture", &queued_task.task_id)
                    .unwrap()
            );
            if !retire_due_first {
                host.retire_schedule(plugin, &due_task, due_slot.id(), true)
                    .unwrap();
            }
            due_slot.abort();
            drop(active);
            queued.await.unwrap();
            // This old-generation worker only requeued AFTER rollback's sweep.
            assert!(
                host.store
                    .pending_schedule("fixture", &queued_task.task_id)
                    .unwrap()
                    .is_some()
            );
            host.retire_schedule(plugin, &queued_task, queued_slot.id(), true)
                .unwrap();
            queued_slot.abort();
            timeout(Duration::from_secs(2), async {
                while probe.schedules.load(Ordering::SeqCst) < 2 {
                    probe.fired.notified().await;
                }
            })
            .await
            .unwrap();
            host.shutdown().await.unwrap();
            assert_eq!(probe.schedules.load(Ordering::SeqCst), 2);
            assert!(host.store.recover_schedules("fixture").unwrap().is_empty());
        }
    }
}

#[tokio::test]
async fn status_projection_tracks_saved_config_grants_and_real_lifecycle_faults() {
    let (host, probe) = fixture().await;
    let installation = crate::PluginInstallation {
        plugin_id: probe.manifest.id.to_string(),
        metadata: probe.manifest.metadata.clone(),
        instance_id: "fixture".to_owned(),
        version: "0.1.0".to_owned(),
        package_path: "not-read.bkm-plugin".to_owned(),
        package_sha256: "0".repeat(64),
        source: "private-source-not-returned".to_owned(),
        trust_level: "local-wasm".to_owned(),
        signature_status: "unsigned".to_owned(),
        requested_permissions: vec![],
        granted_permissions: host.plugins[0]
            .granted_capabilities
            .iter()
            .cloned()
            .collect(),
        config: BTreeMap::new(),
        enabled: true,
        installed_at_ms: 1,
        updated_at_ms: 1,
    };
    host.store.upsert_installation(&installation).unwrap();
    let read = || host.instance_statuses(1).unwrap();
    let runtime = read();
    assert_eq!(runtime.len(), 1);
    assert_eq!(runtime[0].state, PluginInstanceState::Ready);
    let rows = host.store.installation_statuses(&runtime, 1).unwrap();
    assert_eq!(rows[0].config_matches, Some(true));
    assert_eq!(rows[0].grants_match, Some(true));
    assert_eq!(rows[0].name, runtime[0].name);
    let mut changed = installation.clone();
    changed
        .config
        .insert("draft".to_owned(), Value::String("private".to_owned()));
    changed.granted_permissions.clear();
    changed.updated_at_ms = 2;
    host.store.upsert_installation(&changed).unwrap();
    let rows = host.store.installation_statuses(&read(), 1).unwrap();
    assert_eq!(rows[0].config_matches, Some(false));
    assert_eq!(rows[0].grants_match, Some(false));
    assert_eq!(rows[0].updated_at_ms, 2);
    assert!(
        host.store
            .installation_statuses(&read(), 0)
            .unwrap()
            .is_empty()
    );
    assert!(host.instance_statuses(0).unwrap().is_empty());
    host.set_instance_enabled("fixture", false).await.unwrap();
    assert_eq!(read()[0].state, PluginInstanceState::Stopped);
    probe.health.store(2, Ordering::SeqCst);
    assert!(host.set_instance_enabled("fixture", true).await.is_err());
    assert_eq!(read()[0].state, PluginInstanceState::Disabled);
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn status_rejects_oversized_static_instance_and_config() {
    let store = PluginStore::in_memory().unwrap();
    let mut host = StaticPluginHost::new(store);
    let id = "x".repeat(513);
    host.register_trusted(probe(), &id, BTreeMap::new())
        .await
        .unwrap();
    assert!(host.instance_statuses(1).is_err());
    host.shutdown().await.unwrap();
    let (host, _) = fixture().await;
    host.plugins[0]
        .replace_config(BTreeMap::from([(
            "large".to_owned(),
            Value::String("x".repeat(1024 * 1024)),
        )]))
        .unwrap();
    assert!(host.instance_statuses(1).is_err());
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn status_prefix_uses_stable_instance_order_and_marks_unsampled_comparisons_unknown() {
    let mut host = StaticPluginHost::new(PluginStore::in_memory().unwrap());
    let plugin = probe();
    host.register_trusted(plugin.clone(), "b", BTreeMap::new())
        .await
        .unwrap();
    let mut other = probe();
    Arc::get_mut(&mut other).unwrap().manifest.id =
        plugin_api::PluginId::new("dev.bkm.other").unwrap();
    Arc::get_mut(&mut other).unwrap().manifest.commands.clear();
    host.register_trusted(other, "a", BTreeMap::new())
        .await
        .unwrap();
    assert_eq!(host.instance_statuses(1).unwrap()[0].instance_id, "a");
    assert_eq!(host.instance_statuses(2).unwrap()[1].instance_id, "b");
    host.shutdown().await.unwrap();
}
