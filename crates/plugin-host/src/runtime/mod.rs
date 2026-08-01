//! Static BPP plugin runtime, lifecycle, dispatch, and command execution.

mod command;
mod validation;

use command::{action_status_name, execute_context_command, failed_completion};
use validation::{
    event_extensions, event_scope, event_type, requested_capabilities, validate_config_schema,
    validate_output, validate_upgrade_commands,
};

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex as StdMutex, Weak},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use bot_core::{
    Action, Adapter, AdapterError, Context, ContextError, Event, EventHandler, HandlerError,
    MessageTarget, SendMessageAction,
};
use futures_util::FutureExt;
use plugin_api::{
    ActionCompleted, ActionStatus, BPP_VERSION, CommandDeclaration, Disposition, ExtensionPayload,
    HandlerOutput, HealthStatus, HostQueries, HttpRequest, InitContext, PluginCommand, PluginError,
    PluginEventEnvelope, PluginManifest, RuntimeMode, ScheduleCancel, ScheduleCreate,
    ScheduleTriggered, StateOp, StateValue, StaticPlugin,
};
use serde_json::Value;
use thiserror::Error;
use tokio::{
    sync::{Mutex, OwnedMutexGuard, OwnedSemaphorePermit, Semaphore},
    task::AbortHandle,
    time::{sleep, timeout},
};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::{
    CommitOptions, HttpExecutionError, HttpExecutor, PluginStore, SecureHttpExecutor, StoreError,
    storage::{DeliveryFailurePolicy, OutboxOrigin, PendingCommand, ScheduledTask},
};

const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_COMMANDS: usize = 32;
const MAX_STATE_OPS: usize = 128;
const MAX_ACTION_COMPLETION_CHAIN: usize = 64;
const MAX_SCHEDULE_AHEAD_MS: i64 = 366 * 24 * 60 * 60 * 1_000;
const MAX_CONFIG_SCHEMA_BYTES: usize = 256 * 1024;
const MAX_EVENT_EXTENSION_BYTES: usize = 256 * 1024;
const MAX_STATE_SCAN_ENTRIES: usize = 1024;
const MAX_COMMAND_DEADLINE_MS: u64 = 30_000;
const MAX_DELIVERY_ATTEMPTS: u32 = 3;
const CIRCUIT_BREAKER_FAILURES: u32 = 3;
const DELIVERY_RETRY_BASE_MS: u64 = 25;
const ACTION_COMPLETION_DEPTH_NAMESPACE: &str = "dev.bkm.host/action-completion-depth";

#[derive(Debug, Error)]
pub enum PluginHostError {
    #[error("plugin manifest is invalid: {0}")]
    Manifest(String),
    #[error("plugin instance `{0}` is already registered")]
    DuplicateInstance(String),
    #[error("plugin command or alias `{0}` is already registered")]
    DuplicateCommand(String),
    #[error("plugin `{plugin_id}` configuration is invalid: {message}")]
    InvalidConfig { plugin_id: String, message: String },
    #[error("plugin `{plugin_id}` configuration schema is invalid: {message}")]
    InvalidConfigSchema { plugin_id: String, message: String },
    #[error("plugin `{plugin_id}` failed to initialize: {message}")]
    Init { plugin_id: String, message: String },
    #[error("plugin `{plugin_id}` invocation failed: {message}")]
    Invocation { plugin_id: String, message: String },
    #[error("plugin `{plugin_id}` invocation failed transiently: {message}")]
    TransientInvocation { plugin_id: String, message: String },
    #[error("plugin `{plugin_id}` guest trapped: {message}")]
    GuestTrap { plugin_id: String, message: String },
    #[error("plugin `{plugin_id}` exhausted a resource and trapped: {message}")]
    ResourceExhaustedTrap { plugin_id: String, message: String },
    #[error("plugin `{plugin_id}` invocation timed out")]
    Timeout { plugin_id: String },
    #[error("plugin `{plugin_id}` panicked during invocation")]
    Panic { plugin_id: String },
    #[error("plugin `{plugin_id}` returned invalid output: {message}")]
    InvalidOutput { plugin_id: String, message: String },
    #[error("plugin `{plugin_id}` state migration failed: {message}")]
    Migration { plugin_id: String, message: String },
    #[error("plugin `{plugin_id}` changed lifecycle generation while invocation was queued")]
    LifecycleChanged { plugin_id: String },
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginInstanceState {
    Ready,
    Draining,
    Disabled,
    Stopped,
}

#[derive(Clone)]
pub struct StaticPluginHost {
    name: String,
    store: PluginStore,
    plugins: Vec<Arc<RegisteredPlugin>>,
    instances: BTreeSet<String>,
    command_names: BTreeSet<String>,
    http_executor: Arc<dyn HttpExecutor>,
    adapters: Arc<BTreeMap<String, Arc<dyn Adapter>>>,
    scheduled_tasks: Arc<StdMutex<HashMap<(String, String), AbortHandle>>>,
}

impl std::fmt::Debug for StaticPluginHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StaticPluginHost")
            .field("name", &self.name)
            .field("store", &self.store)
            .field("plugin_count", &self.plugins.len())
            .field("instances", &self.instances)
            .field("command_names", &self.command_names)
            .field("http_executor", &"configured")
            .field("adapter_count", &self.adapters.len())
            .field("scheduled_tasks", &"supervised")
            .finish()
    }
}

impl StaticPluginHost {
    pub fn new(store: PluginStore) -> Self {
        Self {
            name: "static-plugin-host".to_owned(),
            store,
            plugins: Vec::new(),
            instances: BTreeSet::new(),
            command_names: BTreeSet::new(),
            http_executor: Arc::new(SecureHttpExecutor),
            adapters: Arc::new(BTreeMap::new()),
            scheduled_tasks: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    #[must_use]
    pub fn with_http_executor(mut self, executor: Arc<dyn HttpExecutor>) -> Self {
        self.http_executor = executor;
        self
    }

    #[must_use]
    pub fn with_adapter(mut self, adapter: Arc<dyn Adapter>) -> Self {
        Arc::make_mut(&mut self.adapters).insert(adapter.id().to_string(), adapter);
        self
    }

    pub async fn register(
        &mut self,
        plugin: Arc<dyn StaticPlugin>,
        instance_id: impl Into<String>,
        config: BTreeMap<String, Value>,
        administrator_grants: BTreeSet<String>,
    ) -> Result<(), PluginHostError> {
        let instance_id = instance_id.into();
        let manifest = plugin.manifest().clone();
        manifest
            .validate()
            .map_err(|error| PluginHostError::Manifest(error.to_string()))?;
        if self.instances.contains(&instance_id) {
            return Err(PluginHostError::DuplicateInstance(instance_id));
        }
        for command in &manifest.commands {
            for name in std::iter::once(&command.name).chain(&command.aliases) {
                if self.command_names.contains(name) {
                    return Err(PluginHostError::DuplicateCommand(name.clone()));
                }
            }
        }

        validate_config_schema(plugin.as_ref(), &manifest.id.to_string(), &config)?;
        supervised_validate_config(plugin.as_ref(), &manifest, &config).await?;
        let granted_capabilities = requested_capabilities(&manifest)
            .intersection(&administrator_grants)
            .cloned()
            .collect::<BTreeSet<_>>();
        let execution = InvocationGate::new(&manifest);
        let registered = Arc::new(RegisteredPlugin {
            plugin,
            manifest,
            instance_id,
            config: StdMutex::new(config.clone()),
            granted_capabilities,
            execution,
            state: StdMutex::new(LifecycleState {
                state: PluginInstanceState::Draining,
                generation: 0,
            }),
        });
        if self.store.circuit_open(&registered.instance_id)? {
            registered.set_state(PluginInstanceState::Disabled)?;
            warn!(
                plugin_id = %registered.manifest.id,
                instance_id = registered.instance_id,
                "registered plugin with an open persistent circuit"
            );
        } else {
            supervised_init(&registered, config).await?;
            registered.set_state(PluginInstanceState::Ready)?;
            self.recover_pending_commands(&registered)?;
            self.recover_pending_deliveries(&registered).await?;
            self.recover_plugin_schedules(&registered)?;
        }
        self.instances.insert(registered.instance_id.clone());
        for command in &registered.manifest.commands {
            self.command_names.insert(command.name.clone());
            self.command_names.extend(command.aliases.iter().cloned());
        }
        info!(plugin_id = %registered.manifest.id, instance_id = registered.instance_id, "registered static plugin");
        self.plugins.push(registered);
        Ok(())
    }

    pub async fn register_trusted(
        &mut self,
        plugin: Arc<dyn StaticPlugin>,
        instance_id: impl Into<String>,
        config: BTreeMap<String, Value>,
    ) -> Result<(), PluginHostError> {
        let grants = requested_capabilities(plugin.manifest());
        self.register(plugin, instance_id, config, grants).await
    }

    pub async fn update_config(
        &self,
        instance_id: &str,
        config: BTreeMap<String, Value>,
    ) -> Result<(), PluginHostError> {
        let plugin = self
            .plugins
            .iter()
            .find(|plugin| plugin.instance_id == instance_id)
            .cloned()
            .ok_or_else(|| PluginHostError::Invocation {
                plugin_id: instance_id.to_owned(),
                message: "plugin instance is not registered".to_owned(),
            })?;
        plugin.begin_draining()?;
        let _execution = plugin.execution.acquire_exclusive().await?;
        let old_config = plugin.config_snapshot()?;
        if let Err(error) = validate_config_schema(
            plugin.plugin.as_ref(),
            &plugin.manifest.id.to_string(),
            &config,
        ) {
            plugin.set_state(PluginInstanceState::Ready)?;
            return Err(error);
        }
        if let Err(error) =
            supervised_validate_config(plugin.plugin.as_ref(), &plugin.manifest, &config).await
        {
            plugin.set_state(PluginInstanceState::Ready)?;
            return Err(error);
        }
        if let Err(error) = supervised_shutdown(&plugin).await {
            plugin.set_state(PluginInstanceState::Ready)?;
            return Err(error);
        }
        match supervised_init(&plugin, config.clone()).await {
            Ok(()) => {
                plugin.replace_config(config)?;
                plugin.set_state(PluginInstanceState::Ready)?;
                Ok(())
            }
            Err(update_error) => match supervised_init(&plugin, old_config).await {
                Ok(()) => {
                    plugin.set_state(PluginInstanceState::Ready)?;
                    Err(update_error)
                }
                Err(rollback_error) => {
                    plugin.set_state(PluginInstanceState::Disabled)?;
                    Err(PluginHostError::Init {
                        plugin_id: plugin.manifest.id.to_string(),
                        message: format!(
                            "new configuration failed ({update_error}); rollback failed ({rollback_error})"
                        ),
                    })
                }
            },
        }
    }

    pub fn instance_state(
        &self,
        instance_id: &str,
    ) -> Result<Option<PluginInstanceState>, PluginHostError> {
        self.plugins
            .iter()
            .find(|plugin| plugin.instance_id == instance_id)
            .map(|plugin| plugin.state())
            .transpose()
    }

    pub fn instance_manifest(&self, instance_id: &str) -> Option<&PluginManifest> {
        self.plugins
            .iter()
            .find(|plugin| plugin.instance_id == instance_id)
            .map(|plugin| &plugin.manifest)
    }

    /// Returns the command declarations from all currently registered plugin
    /// manifests in deterministic plugin registration order.
    pub fn command_declarations(&self) -> Vec<CommandDeclaration> {
        self.plugins
            .iter()
            .flat_map(|plugin| plugin.manifest.commands.iter().cloned())
            .collect()
    }

    pub async fn upgrade_trusted(
        &mut self,
        instance_id: &str,
        plugin: Arc<dyn StaticPlugin>,
        config: BTreeMap<String, Value>,
    ) -> Result<(), PluginHostError> {
        let grants = requested_capabilities(plugin.manifest());
        self.upgrade(instance_id, plugin, config, grants).await
    }

    #[allow(clippy::too_many_lines)]
    pub async fn upgrade(
        &mut self,
        instance_id: &str,
        plugin: Arc<dyn StaticPlugin>,
        config: BTreeMap<String, Value>,
        administrator_grants: BTreeSet<String>,
    ) -> Result<(), PluginHostError> {
        let Some(index) = self
            .plugins
            .iter()
            .position(|registered| registered.instance_id == instance_id)
        else {
            return Err(PluginHostError::Invocation {
                plugin_id: instance_id.to_owned(),
                message: "plugin instance is not registered".to_owned(),
            });
        };
        let old = Arc::clone(&self.plugins[index]);
        let manifest = plugin.manifest().clone();
        manifest
            .validate()
            .map_err(|error| PluginHostError::Manifest(error.to_string()))?;
        if manifest.id != old.manifest.id {
            return Err(PluginHostError::Manifest(
                "an upgrade cannot change plugin ID".to_owned(),
            ));
        }
        if manifest.state_version < old.manifest.state_version {
            return Err(PluginHostError::Manifest(
                "an upgrade cannot decrease state_version".to_owned(),
            ));
        }
        validate_upgrade_commands(&self.plugins, index, &manifest)?;
        validate_config_schema(plugin.as_ref(), &manifest.id.to_string(), &config)?;
        supervised_validate_config(plugin.as_ref(), &manifest, &config).await?;
        let granted_capabilities = requested_capabilities(&manifest)
            .intersection(&administrator_grants)
            .cloned()
            .collect::<BTreeSet<_>>();
        let execution = InvocationGate::new(&manifest);
        let candidate = Arc::new(RegisteredPlugin {
            plugin,
            manifest,
            instance_id: instance_id.to_owned(),
            config: StdMutex::new(config.clone()),
            granted_capabilities,
            execution,
            state: StdMutex::new(LifecycleState {
                state: PluginInstanceState::Draining,
                generation: 0,
            }),
        });

        old.begin_draining()?;
        let _execution = old.execution.acquire_exclusive().await?;
        self.pause_plugin_schedules(instance_id)?;
        let old_config = old.config_snapshot()?;
        let old_state = self.store.snapshot(instance_id)?;
        if let Err(error) = supervised_shutdown(&old).await {
            old.set_state(PluginInstanceState::Ready)?;
            self.recover_plugin_schedules(&old)?;
            return Err(error);
        }

        let upgrade_result = async {
            if candidate.manifest.state_version != old.manifest.state_version {
                let operations = supervised_migration(
                    &candidate,
                    old.manifest.state_version,
                    candidate.manifest.state_version,
                    &old_state,
                )
                .await?;
                validate_output(
                    &candidate,
                    &HandlerOutput {
                        disposition: Disposition::Continue,
                        state_ops: operations.clone(),
                        commands: Vec::new(),
                        diagnostics: Vec::new(),
                    },
                )?;
                self.store.commit(
                    instance_id,
                    &format!("migration-{}", Uuid::new_v4()),
                    &operations,
                    &[],
                    CommitOptions::new(candidate.manifest.permissions.storage_quota_bytes),
                )?;
            }
            supervised_init(&candidate, config).await?;
            if let Err(error) = supervised_health(&candidate).await {
                if let Err(shutdown_error) = supervised_shutdown(&candidate).await {
                    warn!(
                        plugin_id = %candidate.manifest.id,
                        error = %shutdown_error,
                        "failed to shut down rejected upgrade candidate"
                    );
                }
                return Err(error);
            }
            Ok(())
        }
        .await;

        if let Err(error) = upgrade_result {
            return self
                .rollback_upgrade(&old, &old_state, old_config, error)
                .await;
        }

        candidate.set_state(PluginInstanceState::Ready)?;
        self.plugins[index] = Arc::clone(&candidate);
        self.rebuild_command_names();
        self.recover_plugin_schedules(&candidate)?;
        Ok(())
    }

    async fn rollback_upgrade(
        &self,
        old: &Arc<RegisteredPlugin>,
        old_state: &BTreeMap<String, StateValue>,
        old_config: BTreeMap<String, Value>,
        upgrade_error: PluginHostError,
    ) -> Result<(), PluginHostError> {
        self.store.replace_state(&old.instance_id, old_state)?;
        match supervised_init(old, old_config).await {
            Ok(()) => {
                old.set_state(PluginInstanceState::Ready)?;
                self.recover_plugin_schedules(old)?;
                Err(upgrade_error)
            }
            Err(rollback_error) => {
                old.set_state(PluginInstanceState::Disabled)?;
                Err(PluginHostError::Migration {
                    plugin_id: old.manifest.id.to_string(),
                    message: format!(
                        "upgrade failed ({upgrade_error}); old plugin restart failed ({rollback_error})"
                    ),
                })
            }
        }
    }

    async fn dispatch(&self, context: Context, event: &Event) -> Result<(), PluginHostError> {
        let event_type = event_type(event);
        let scope = event_scope(event);
        let mut plugins = self
            .plugins
            .iter()
            .filter_map(|plugin| {
                plugin
                    .priority_for(event_type, scope)
                    .map(|priority| (priority, plugin))
            })
            .collect::<Vec<_>>();
        plugins.sort_by_key(|(priority, plugin)| (*priority, plugin.manifest.id.clone()));

        for (_, plugin) in plugins {
            let result = async {
                let Some(generation) = plugin.ready_generation()? else {
                    return Ok(None);
                };
                let _execution = plugin
                    .execution
                    .acquire(event_partition_key(event, context.event_id().as_str()))
                    .await?;
                if !plugin.is_ready_generation(generation)? {
                    warn!(plugin_id = %plugin.manifest.id, "plugin lifecycle changed while invocation was queued");
                    return Ok(None);
                }
                let envelope = Self::event_envelope(&context, event, event_type)?;
                self.deliver_with_retry(plugin, ExecutionOrigin::Event(&context), envelope)
                    .await
                    .map(Some)
            }
            .await;
            match result {
                Ok(Some(true)) => break,
                Ok(Some(false) | None) => {}
                Err(error) => error!(
                    plugin_id = %plugin.manifest.id,
                    instance_id = plugin.instance_id,
                    error = %error,
                    "isolated plugin host delivery failure"
                ),
            }
        }
        Ok(())
    }

    fn event_envelope(
        context: &Context,
        event: &Event,
        event_type: &str,
    ) -> Result<PluginEventEnvelope, PluginHostError> {
        let invocation_id = Uuid::new_v4().to_string();
        let now = now_ms();
        Ok(PluginEventEnvelope {
            protocol_version: BPP_VERSION.to_owned(),
            event_id: context.event_id().to_string(),
            delivery_id: Uuid::new_v4().to_string(),
            invocation_id,
            occurred_at_ms: context.occurred_at_ms(),
            received_at_ms: now,
            adapter_id: context.adapter_id().to_string(),
            event_type: event_type.to_owned(),
            trace_id: None,
            payload: serde_json::to_value(event).map_err(|error| PluginHostError::Invocation {
                plugin_id: "event-envelope".to_owned(),
                message: error.to_string(),
            })?,
            extensions: Vec::new(),
        })
    }

    async fn deliver_with_retry(
        &self,
        plugin: &Arc<RegisteredPlugin>,
        origin: ExecutionOrigin<'_>,
        envelope: PluginEventEnvelope,
    ) -> Result<bool, PluginHostError> {
        let (stop, completions) = self
            .deliver_single_with_retry(plugin, origin, envelope)
            .await?;
        if let Err(error) = self
            .deliver_action_completions(plugin, origin, completions)
            .await
        {
            error!(
                plugin_id = %plugin.manifest.id,
                instance_id = plugin.instance_id,
                error = %error,
                "action-completion delivery will be recovered later"
            );
        }
        Ok(stop)
    }

    async fn deliver_action_completions(
        &self,
        plugin: &Arc<RegisteredPlugin>,
        origin: ExecutionOrigin<'_>,
        completions: VecDeque<PluginEventEnvelope>,
    ) -> Result<(), PluginHostError> {
        let mut queue = completions;
        while let Some(envelope) = queue.pop_front() {
            let (_, nested) = self
                .deliver_single_with_retry(plugin, origin, envelope)
                .await?;
            queue.extend(nested);
            if plugin.state()? == PluginInstanceState::Disabled {
                break;
            }
        }
        Ok(())
    }

    async fn deliver_single_with_retry(
        &self,
        plugin: &Arc<RegisteredPlugin>,
        origin: ExecutionOrigin<'_>,
        mut envelope: PluginEventEnvelope,
    ) -> Result<(bool, VecDeque<PluginEventEnvelope>), PluginHostError> {
        if self.store.circuit_open(&plugin.instance_id)? {
            plugin.set_state(PluginInstanceState::Disabled)?;
            return Ok((false, VecDeque::new()));
        }
        loop {
            envelope.delivery_id = Uuid::new_v4().to_string();
            envelope.invocation_id = Uuid::new_v4().to_string();
            if let ExecutionOrigin::Event(context) = origin
                && envelope.event_type != "action.completed"
            {
                envelope.extensions = event_extensions(plugin, context)?;
            }
            let outbox_origin = origin.outbox_origin(&envelope.event_id);
            if self.reject_excessive_action_completion(plugin, &envelope, &outbox_origin)? {
                return Ok((false, VecDeque::new()));
            }
            let Some(delivery) = self.store.begin_delivery(
                &plugin.instance_id,
                &envelope,
                &outbox_origin,
                now_ms(),
            )?
            else {
                return Ok((false, VecDeque::new()));
            };
            let result = async {
                let output = self.invoke_envelope(plugin, &envelope).await?;
                let stop = output.disposition == Disposition::Stop;
                let completions = self
                    .commit_and_execute(
                        plugin,
                        origin,
                        &envelope.event_id,
                        &envelope.delivery_id,
                        &envelope.invocation_id,
                        output,
                    )
                    .await?;
                Ok::<_, PluginHostError>((stop, completions))
            }
            .await;
            match result {
                Ok((stop, completions)) => {
                    let completion_envelopes = Self::prepare_action_completion_envelopes(
                        plugin,
                        origin,
                        completions,
                        action_completion_depth(&envelope),
                    )?;
                    let completed_at_ms = now_ms();
                    for completion in &completion_envelopes {
                        self.store.enqueue_delivery(
                            &plugin.instance_id,
                            completion,
                            &origin.outbox_origin(&completion.event_id),
                            completed_at_ms,
                        )?;
                    }
                    if !self.store.mark_delivery_succeeded(
                        &plugin.instance_id,
                        &envelope.event_id,
                        &envelope.delivery_id,
                        completed_at_ms,
                    )? {
                        return Ok((false, VecDeque::new()));
                    }
                    return Ok((stop, completion_envelopes));
                }
                Err(error) => {
                    let Some(delay_ms) =
                        self.record_delivery_failure(plugin, &envelope, delivery.attempt, &error)?
                    else {
                        let recovered =
                            self.recover_committed_delivery(plugin, origin, &envelope)?;
                        return Ok((false, recovered));
                    };
                    sleep(Duration::from_millis(delay_ms)).await;
                }
            }
        }
    }

    fn reject_excessive_action_completion(
        &self,
        plugin: &RegisteredPlugin,
        envelope: &PluginEventEnvelope,
        outbox_origin: &OutboxOrigin,
    ) -> Result<bool, PluginHostError> {
        if envelope.event_type != "action.completed"
            || action_completion_depth(envelope) <= MAX_ACTION_COMPLETION_CHAIN
        {
            return Ok(false);
        }
        let now = now_ms();
        if let Some(delivery) =
            self.store
                .begin_delivery(&plugin.instance_id, envelope, outbox_origin, now)?
        {
            self.store.mark_delivery_failed(
                &plugin.instance_id,
                &envelope.event_id,
                &envelope.delivery_id,
                "action completion chain exceeded limit",
                DeliveryFailurePolicy {
                    next_attempt_ms: now,
                    max_attempts: delivery.attempt,
                    circuit_threshold: 1,
                    counts_toward_circuit: true,
                    now_ms: now,
                },
            )?;
        }
        error!(
            plugin_id = %plugin.manifest.id,
            instance_id = plugin.instance_id,
            "disabled plugin after action completion chain exceeded limit"
        );
        plugin.set_state(PluginInstanceState::Disabled)?;
        Ok(true)
    }

    fn record_delivery_failure(
        &self,
        plugin: &RegisteredPlugin,
        envelope: &PluginEventEnvelope,
        attempt: u32,
        error: &PluginHostError,
    ) -> Result<Option<u64>, PluginHostError> {
        let retryable = delivery_error_retryable(error);
        let delay_ms = delivery_retry_delay_ms(attempt);
        let max_attempts = if retryable { MAX_DELIVERY_ATTEMPTS } else { 1 };
        let fatal_trap = matches!(
            error,
            PluginHostError::GuestTrap { .. } | PluginHostError::ResourceExhaustedTrap { .. }
        );
        let failure_time = now_ms();
        let failure = self.store.mark_delivery_failed(
            &plugin.instance_id,
            &envelope.event_id,
            &envelope.delivery_id,
            &error.to_string(),
            DeliveryFailurePolicy {
                next_attempt_ms: failure_time
                    .saturating_add(i64::try_from(delay_ms).unwrap_or(i64::MAX)),
                max_attempts,
                circuit_threshold: if fatal_trap {
                    1
                } else if retryable {
                    CIRCUIT_BREAKER_FAILURES
                } else {
                    u32::MAX
                },
                counts_toward_circuit: fatal_trap || retryable,
                now_ms: failure_time,
            },
        )?;
        if !failure.updated {
            return Ok(None);
        }
        let dead_letter = failure.dead_letter;
        let circuit_open = failure.circuit_open;
        error!(
            plugin_id = %plugin.manifest.id,
            instance_id = plugin.instance_id,
            event_id = envelope.event_id,
            attempt,
            dead_letter,
            circuit_open,
            error = %error,
            "isolated plugin delivery failure"
        );
        if fatal_trap || circuit_open {
            plugin.set_state(PluginInstanceState::Disabled)?;
        }
        Ok((!dead_letter && !circuit_open && retryable).then_some(delay_ms))
    }

    fn prepare_action_completion_envelopes(
        plugin: &RegisteredPlugin,
        origin: ExecutionOrigin<'_>,
        completions: VecDeque<ActionCompleted>,
        parent_depth: usize,
    ) -> Result<VecDeque<PluginEventEnvelope>, PluginHostError> {
        if plugin.priority_for("action.completed", None).is_none() {
            return Ok(VecDeque::new());
        }
        completions
            .into_iter()
            .map(|completion| {
                Self::action_completion_envelope(origin, completion, parent_depth.saturating_add(1))
            })
            .collect()
    }

    fn recover_committed_delivery(
        &self,
        plugin: &RegisteredPlugin,
        origin: ExecutionOrigin<'_>,
        envelope: &PluginEventEnvelope,
    ) -> Result<VecDeque<PluginEventEnvelope>, PluginHostError> {
        for pending in self
            .store
            .pending_commands(&plugin.instance_id)?
            .into_iter()
            .filter(|pending| pending.invocation_id == envelope.invocation_id)
        {
            let completion = interrupted_completion(&pending);
            let persisted =
                serde_json::to_value(&completion).map_err(|error| PluginHostError::Invocation {
                    plugin_id: plugin.manifest.id.to_string(),
                    message: error.to_string(),
                })?;
            self.store.mark_command(
                &pending.instance_id,
                &pending.invocation_id,
                &pending.command.command_id,
                action_status_name(completion.status),
                &persisted,
            )?;
        }
        let mut recovered = VecDeque::new();
        for (_, completion, _) in self
            .store
            .committed_command_results(&plugin.instance_id)?
            .into_iter()
            .filter(|(pending, _, _)| pending.invocation_id == envelope.invocation_id)
        {
            let completions = Self::prepare_action_completion_envelopes(
                plugin,
                origin,
                VecDeque::from([completion]),
                action_completion_depth(envelope),
            )?;
            for completion in &completions {
                self.store.enqueue_delivery(
                    &plugin.instance_id,
                    completion,
                    &origin.outbox_origin(&completion.event_id),
                    now_ms(),
                )?;
            }
            recovered.extend(completions);
        }
        self.store.finalize_committed_delivery(
            &plugin.instance_id,
            &envelope.event_id,
            now_ms(),
        )?;
        Ok(recovered)
    }

    fn action_completion_envelope(
        origin: ExecutionOrigin<'_>,
        completion: ActionCompleted,
        depth: usize,
    ) -> Result<PluginEventEnvelope, PluginHostError> {
        let event_id = format!(
            "action/{}/{}",
            completion.source_invocation_id, completion.command_id
        );
        let now = now_ms();
        Ok(PluginEventEnvelope {
            protocol_version: BPP_VERSION.to_owned(),
            event_id,
            delivery_id: Uuid::new_v4().to_string(),
            invocation_id: Uuid::new_v4().to_string(),
            occurred_at_ms: Some(now),
            received_at_ms: now,
            adapter_id: origin.adapter_id().to_owned(),
            event_type: "action.completed".to_owned(),
            trace_id: None,
            payload: serde_json::to_value(completion).map_err(|error| {
                PluginHostError::Invocation {
                    plugin_id: "action-completed".to_owned(),
                    message: error.to_string(),
                }
            })?,
            extensions: vec![ExtensionPayload {
                namespace: ACTION_COMPLETION_DEPTH_NAMESPACE.to_owned(),
                schema_version: "1.0.0".to_owned(),
                content_type: "text/plain".to_owned(),
                data: depth.to_string().into_bytes(),
            }],
        })
    }

    async fn recover_pending_deliveries(
        &self,
        plugin: &Arc<RegisteredPlugin>,
    ) -> Result<(), PluginHostError> {
        self.store
            .requeue_running_deliveries(&plugin.instance_id, now_ms())?;
        let mut pending = self
            .store
            .pending_deliveries(&plugin.instance_id, i64::MAX)?;
        pending.sort_by_key(|delivery| (delivery.next_attempt_ms, delivery.event_id.clone()));
        for pending in pending {
            let recovery_wait_ms = u64::try_from(pending.next_attempt_ms.saturating_sub(now_ms()))
                .unwrap_or_default()
                .min(delivery_retry_delay_ms(pending.attempt.max(1)));
            if recovery_wait_ms > 0 {
                sleep(Duration::from_millis(recovery_wait_ms)).await;
            }
            let mut envelope = pending.envelope;
            envelope.delivery_id = Uuid::new_v4().to_string();
            envelope.invocation_id = Uuid::new_v4().to_string();
            self.deliver_with_retry(
                plugin,
                ExecutionOrigin::Recovered(&pending.origin),
                envelope,
            )
            .await?;
            if plugin.state()? == PluginInstanceState::Disabled {
                break;
            }
        }
        Ok(())
    }

    async fn invoke_envelope(
        &self,
        plugin: &RegisteredPlugin,
        envelope: &PluginEventEnvelope,
    ) -> Result<HandlerOutput, PluginHostError> {
        let state = self.store.snapshot(&plugin.instance_id)?;
        let config = plugin.config_snapshot()?;
        let queries = InvocationQueries {
            config: &config,
            state: &state,
            granted_capabilities: &plugin.granted_capabilities,
            invocation_time_ms: envelope.received_at_ms,
        };
        let invocation =
            AssertUnwindSafe(plugin.plugin.on_event(envelope, &queries)).catch_unwind();
        let output = timeout(plugin.manifest.timeout(), invocation)
            .await
            .map_err(|_| PluginHostError::Timeout {
                plugin_id: plugin.manifest.id.to_string(),
            })?
            .map_err(|_| PluginHostError::Panic {
                plugin_id: plugin.manifest.id.to_string(),
            })?
            .map_err(|error| match error {
                PluginError::GuestTrap(message) => PluginHostError::GuestTrap {
                    plugin_id: plugin.manifest.id.to_string(),
                    message,
                },
                PluginError::ResourceExhaustedTrap(message) => {
                    PluginHostError::ResourceExhaustedTrap {
                        plugin_id: plugin.manifest.id.to_string(),
                        message,
                    }
                }
                PluginError::Transient(message) => PluginHostError::TransientInvocation {
                    plugin_id: plugin.manifest.id.to_string(),
                    message,
                },
                error => PluginHostError::Invocation {
                    plugin_id: plugin.manifest.id.to_string(),
                    message: error.to_string(),
                },
            })?;
        validate_output(plugin, &output)?;
        Ok(output)
    }

    async fn commit_and_execute(
        &self,
        plugin: &Arc<RegisteredPlugin>,
        origin: ExecutionOrigin<'_>,
        source_event_id: &str,
        delivery_id: &str,
        invocation_id: &str,
        output: HandlerOutput,
    ) -> Result<VecDeque<ActionCompleted>, PluginHostError> {
        for diagnostic in &output.diagnostics {
            warn!(
                plugin_id = %plugin.manifest.id,
                code = diagnostic.code,
                level = diagnostic.level,
                message = diagnostic.message,
                "plugin diagnostic"
            );
        }
        let outbox_origin = origin.outbox_origin(source_event_id);
        let commands = self.store.commit(
            &plugin.instance_id,
            invocation_id,
            &output.state_ops,
            &output.commands,
            CommitOptions::new(plugin.manifest.permissions.storage_quota_bytes)
                .with_origin(&outbox_origin)
                .with_delivery(source_event_id, delivery_id),
        )?;
        let mut completions = VecDeque::new();
        for command in commands {
            completions.push_back(
                self.execute_command(plugin, origin, source_event_id, invocation_id, command)
                    .await?,
            );
        }
        Ok(completions)
    }

    async fn execute_command(
        &self,
        plugin: &Arc<RegisteredPlugin>,
        origin: ExecutionOrigin<'_>,
        source_event_id: &str,
        invocation_id: &str,
        command: PluginCommand,
    ) -> Result<ActionCompleted, PluginHostError> {
        let execution = async {
            match command.kind.as_str() {
                "schedule.create" | "schedule.cancel" => {
                    self.execute_schedule_command(plugin, origin, &command)
                }
                _ => {
                    execute_context_command(
                        origin,
                        &command,
                        self.http_executor.as_ref(),
                        &plugin.manifest,
                        &plugin.granted_capabilities,
                        &self.adapters,
                    )
                    .await
                }
            }
        };
        let result = if let Some(deadline_ms) = command.deadline_ms {
            timeout(Duration::from_millis(deadline_ms), execution)
                .await
                .ok()
        } else {
            Some(execution.await)
        };
        let completion = match result {
            None => ActionCompleted {
                source_event_id: source_event_id.to_owned(),
                source_invocation_id: invocation_id.to_owned(),
                command_id: command.command_id.clone(),
                kind: command.kind.clone(),
                status: ActionStatus::Unknown,
                retryable: false,
                result: None,
                error_code: Some("deadline_exceeded".to_owned()),
                error_message: Some("command execution exceeded its deadline".to_owned()),
            },
            Some(Ok(value)) => ActionCompleted {
                source_event_id: source_event_id.to_owned(),
                source_invocation_id: invocation_id.to_owned(),
                command_id: command.command_id.clone(),
                kind: command.kind.clone(),
                status: ActionStatus::Succeeded,
                retryable: false,
                result: Some(value),
                error_code: None,
                error_message: None,
            },
            Some(Err(error)) => {
                error!(
                    plugin_id = %plugin.manifest.id,
                    command_id = command.command_id,
                    error = %error,
                    "plugin command failed"
                );
                failed_completion(source_event_id, invocation_id, &command, &error)
            }
        };
        let persisted =
            serde_json::to_value(&completion).map_err(|error| PluginHostError::Invocation {
                plugin_id: plugin.manifest.id.to_string(),
                message: error.to_string(),
            })?;
        self.store.mark_command(
            &plugin.instance_id,
            invocation_id,
            &command.command_id,
            action_status_name(completion.status),
            &persisted,
        )?;
        Ok(completion)
    }

    fn execute_schedule_command(
        &self,
        plugin: &Arc<RegisteredPlugin>,
        origin: ExecutionOrigin<'_>,
        command: &PluginCommand,
    ) -> Result<Value, PluginError> {
        match command.kind.as_str() {
            "schedule.create" => {
                let create: ScheduleCreate = serde_json::from_value(command.payload.clone())
                    .map_err(|error| PluginError::Permanent(error.to_string()))?;
                let now = now_ms();
                if create.run_at_ms > now.saturating_add(MAX_SCHEDULE_AHEAD_MS) {
                    return Err(PluginError::Permanent(
                        "scheduled task is more than 366 days in the future".to_owned(),
                    ));
                }
                let task = ScheduledTask {
                    instance_id: plugin.instance_id.clone(),
                    task_id: create.task_id,
                    adapter_id: origin.adapter_id().to_owned(),
                    run_at_ms: create.run_at_ms,
                    payload: create.payload,
                };
                let created = self
                    .store
                    .create_schedule(&task, plugin.manifest.permissions.storage_quota_bytes)
                    .map_err(|error| PluginError::Permanent(error.to_string()))?;
                if created {
                    self.launch_schedule(plugin, &task);
                }
                Ok(serde_json::json!({
                    "created": created,
                    "task_id": task.task_id,
                    "run_at_ms": task.run_at_ms,
                }))
            }
            "schedule.cancel" => {
                let cancel: ScheduleCancel = serde_json::from_value(command.payload.clone())
                    .map_err(|error| PluginError::Permanent(error.to_string()))?;
                let task_id = cancel.task_id.clone();
                let cancelled = self
                    .store
                    .cancel_schedule(&plugin.instance_id, &cancel.task_id)
                    .map_err(|error| PluginError::Permanent(error.to_string()))?;
                if cancelled {
                    self.abort_schedule(&plugin.instance_id, &cancel.task_id)?;
                }
                Ok(serde_json::json!({"cancelled":cancelled,"task_id":task_id}))
            }
            _ => Err(PluginError::Permanent(
                "unsupported scheduler command".to_owned(),
            )),
        }
    }

    fn recover_plugin_schedules(
        &self,
        plugin: &Arc<RegisteredPlugin>,
    ) -> Result<(), PluginHostError> {
        for task in self.store.recover_schedules(&plugin.instance_id)? {
            self.launch_schedule(plugin, &task);
        }
        Ok(())
    }

    fn recover_pending_commands(
        &self,
        plugin: &Arc<RegisteredPlugin>,
    ) -> Result<(), PluginHostError> {
        for pending in self.store.pending_commands(&plugin.instance_id)? {
            let completion = interrupted_completion(&pending);
            let persisted =
                serde_json::to_value(&completion).map_err(|error| PluginHostError::Invocation {
                    plugin_id: plugin.manifest.id.to_string(),
                    message: error.to_string(),
                })?;
            self.store.mark_command(
                &pending.instance_id,
                &pending.invocation_id,
                &pending.command.command_id,
                action_status_name(completion.status),
                &persisted,
            )?;
        }
        for (pending, completion, parent_envelope) in
            self.store.committed_command_results(&plugin.instance_id)?
        {
            let origin = ExecutionOrigin::Recovered(&pending.origin);
            let completions = Self::prepare_action_completion_envelopes(
                plugin,
                origin,
                VecDeque::from([completion]),
                action_completion_depth(&parent_envelope),
            )?;
            for envelope in &completions {
                self.store.enqueue_delivery(
                    &plugin.instance_id,
                    envelope,
                    &origin.outbox_origin(&envelope.event_id),
                    now_ms(),
                )?;
            }
        }
        self.store
            .finalize_committed_deliveries(&plugin.instance_id, now_ms())?;
        Ok(())
    }

    fn launch_schedule(&self, plugin: &Arc<RegisteredPlugin>, task: &ScheduledTask) {
        let key = (task.instance_id.clone(), task.task_id.clone());
        let Ok(mut handles) = self.scheduled_tasks.lock() else {
            error!(
                instance_id = task.instance_id,
                task_id = task.task_id,
                "scheduler lock is poisoned"
            );
            return;
        };
        if handles.contains_key(&key) {
            return;
        }
        let host = self.clone();
        let plugin = Arc::clone(plugin);
        let task_for_worker = task.clone();
        let key_for_worker = key.clone();
        let worker = tokio::spawn(async move {
            let delay_ms = task_for_worker.run_at_ms.saturating_sub(now_ms()).max(0);
            sleep(Duration::from_millis(
                u64::try_from(delay_ms).unwrap_or(u64::MAX),
            ))
            .await;
            let claimed = host
                .store
                .claim_schedule(&task_for_worker.instance_id, &task_for_worker.task_id);
            let (was_claimed, result) = match claimed {
                Ok(true) => match plugin
                    .execution
                    .acquire(schedule_partition_key(&task_for_worker))
                    .await
                {
                    Ok(_execution) => (true, host.fire_schedule(&plugin, &task_for_worker).await),
                    Err(error) => (true, Err(error)),
                },
                Ok(false) => (false, Ok(())),
                Err(error) => (false, Err(PluginHostError::Store(error))),
            };
            if was_claimed {
                let status = if result.is_ok() {
                    "completed"
                } else {
                    "failed"
                };
                if let Err(error) = host.store.finish_schedule(
                    &task_for_worker.instance_id,
                    &task_for_worker.task_id,
                    status,
                ) {
                    error!(instance_id = task_for_worker.instance_id, task_id = task_for_worker.task_id, error = %error, "failed to persist scheduler completion");
                }
            }
            if let Err(error) = result {
                error!(instance_id = task_for_worker.instance_id, task_id = task_for_worker.task_id, error = %error, "scheduled plugin invocation failed");
            }
            if let Ok(mut handles) = host.scheduled_tasks.lock() {
                handles.remove(&key_for_worker);
            }
        });
        handles.insert(key, worker.abort_handle());
    }

    async fn fire_schedule(
        &self,
        plugin: &Arc<RegisteredPlugin>,
        task: &ScheduledTask,
    ) -> Result<(), PluginHostError> {
        if plugin.state()? != PluginInstanceState::Ready {
            return Err(PluginHostError::Invocation {
                plugin_id: plugin.manifest.id.to_string(),
                message: "plugin instance is not ready for scheduled delivery".to_owned(),
            });
        }
        if plugin.priority_for("schedule.triggered", None).is_none() {
            return Err(PluginHostError::InvalidOutput {
                plugin_id: plugin.manifest.id.to_string(),
                message: "plugin created a schedule without subscribing to schedule.triggered"
                    .to_owned(),
            });
        }
        let event_id = format!("schedule/{}/{}", task.instance_id, task.task_id);
        let now = now_ms();
        let envelope = PluginEventEnvelope {
            protocol_version: BPP_VERSION.to_owned(),
            event_id: event_id.clone(),
            delivery_id: Uuid::new_v4().to_string(),
            invocation_id: Uuid::new_v4().to_string(),
            occurred_at_ms: Some(task.run_at_ms),
            received_at_ms: now,
            adapter_id: task.adapter_id.clone(),
            event_type: "schedule.triggered".to_owned(),
            trace_id: None,
            payload: serde_json::to_value(ScheduleTriggered {
                task_id: task.task_id.clone(),
                scheduled_at_ms: task.run_at_ms,
                payload: task.payload.clone(),
            })
            .map_err(|error| PluginHostError::Invocation {
                plugin_id: plugin.manifest.id.to_string(),
                message: error.to_string(),
            })?,
            extensions: Vec::new(),
        };
        let origin = ExecutionOrigin::Scheduled {
            adapter_id: &task.adapter_id,
        };
        self.deliver_with_retry(plugin, origin, envelope)
            .await
            .map(|_| ())
    }

    fn abort_schedule(&self, instance_id: &str, task_id: &str) -> Result<(), PluginError> {
        let mut handles = self
            .scheduled_tasks
            .lock()
            .map_err(|_| PluginError::Transient("scheduler lock is poisoned".to_owned()))?;
        if let Some(handle) = handles.remove(&(instance_id.to_owned(), task_id.to_owned())) {
            handle.abort();
        }
        Ok(())
    }

    fn pause_plugin_schedules(&self, instance_id: &str) -> Result<(), PluginHostError> {
        let mut handles = self
            .scheduled_tasks
            .lock()
            .map_err(|_| PluginHostError::Invocation {
                plugin_id: instance_id.to_owned(),
                message: "scheduler lock is poisoned".to_owned(),
            })?;
        let keys = handles
            .keys()
            .filter(|(scheduled_instance, _)| scheduled_instance == instance_id)
            .cloned()
            .collect::<Vec<_>>();
        for key in keys {
            if let Some(handle) = handles.remove(&key) {
                handle.abort();
            }
        }
        self.store.requeue_firing_schedules(instance_id)?;
        Ok(())
    }

    fn rebuild_command_names(&mut self) {
        self.command_names.clear();
        for plugin in &self.plugins {
            for command in &plugin.manifest.commands {
                self.command_names.insert(command.name.clone());
                self.command_names.extend(command.aliases.iter().cloned());
            }
        }
    }

    pub fn stop_schedulers(&self) -> Result<(), PluginHostError> {
        let mut handles = self
            .scheduled_tasks
            .lock()
            .map_err(|_| PluginHostError::Invocation {
                plugin_id: "scheduler".to_owned(),
                message: "scheduler lock is poisoned".to_owned(),
            })?;
        for (_, handle) in handles.drain() {
            handle.abort();
        }
        for plugin in &self.plugins {
            self.store.requeue_firing_schedules(&plugin.instance_id)?;
        }
        Ok(())
    }

    /// Drains all ready plugin instances, stops scheduled work, and invokes
    /// each plugin's bounded BPP shutdown lifecycle hook.
    pub async fn shutdown(&self) -> Result<(), PluginHostError> {
        self.stop_schedulers()?;
        let mut first_error = None;
        for plugin in &self.plugins {
            match plugin.state()? {
                PluginInstanceState::Ready => plugin.begin_draining()?,
                PluginInstanceState::Draining => {}
                PluginInstanceState::Disabled | PluginInstanceState::Stopped => continue,
            }
            let execution = plugin.execution.acquire_exclusive().await;
            match execution {
                Ok(_execution) => match supervised_shutdown(plugin).await {
                    Ok(()) => plugin.set_state(PluginInstanceState::Stopped)?,
                    Err(error) => {
                        plugin.set_state(PluginInstanceState::Disabled)?;
                        if first_error.is_none() {
                            first_error = Some(error);
                        }
                    }
                },
                Err(error) => {
                    plugin.set_state(PluginInstanceState::Disabled)?;
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

#[async_trait]
impl EventHandler for StaticPluginHost {
    fn name(&self) -> &str {
        &self.name
    }

    async fn handle(&self, context: Context, event: &Event) -> Result<(), HandlerError> {
        self.dispatch(context, event)
            .await
            .map_err(|error| HandlerError::Failed(error.to_string()))
    }
}

#[derive(Clone, Copy)]
enum ExecutionOrigin<'a> {
    Event(&'a Context),
    Scheduled { adapter_id: &'a str },
    Recovered(&'a OutboxOrigin),
}

impl<'a> ExecutionOrigin<'a> {
    fn adapter_id(self) -> &'a str {
        match self {
            Self::Event(context) => context.adapter_id().as_str(),
            Self::Scheduled { adapter_id } => adapter_id,
            Self::Recovered(origin) => &origin.adapter_id,
        }
    }

    fn outbox_origin(self, source_event_id: &str) -> OutboxOrigin {
        match self {
            Self::Event(context) => OutboxOrigin {
                source_event_id: source_event_id.to_owned(),
                adapter_id: context.adapter_id().to_string(),
                reply_target: context.reply_target().cloned(),
                source_message_id: context.source_message_id().map(str::to_owned),
            },
            Self::Scheduled { adapter_id } => OutboxOrigin {
                source_event_id: source_event_id.to_owned(),
                adapter_id: adapter_id.to_owned(),
                reply_target: None,
                source_message_id: None,
            },
            Self::Recovered(origin) => origin.clone(),
        }
    }
}

fn interrupted_completion(pending: &PendingCommand) -> ActionCompleted {
    ActionCompleted {
        source_event_id: pending.origin.source_event_id.clone(),
        source_invocation_id: pending.invocation_id.clone(),
        command_id: pending.command.command_id.clone(),
        kind: pending.command.kind.clone(),
        status: ActionStatus::Unknown,
        retryable: false,
        result: None,
        error_code: Some("interrupted_before_result".to_owned()),
        error_message: Some(
            "host restarted before the command result was durably recorded".to_owned(),
        ),
    }
}

struct InvocationGate {
    plugin_id: String,
    mode: RuntimeMode,
    max_concurrency: u32,
    permits: Arc<Semaphore>,
    partitions: StdMutex<HashMap<String, Weak<Mutex<()>>>>,
}

impl InvocationGate {
    fn new(manifest: &PluginManifest) -> Self {
        let max_concurrency = if manifest.runtime.mode == RuntimeMode::Serial {
            1
        } else {
            manifest.runtime.max_concurrency
        };
        Self {
            plugin_id: manifest.id.to_string(),
            mode: manifest.runtime.mode,
            max_concurrency,
            permits: Arc::new(Semaphore::new(
                usize::try_from(max_concurrency).unwrap_or(usize::MAX),
            )),
            partitions: StdMutex::new(HashMap::new()),
        }
    }

    async fn acquire(&self, partition_key: String) -> Result<InvocationGuard, PluginHostError> {
        let partition = if self.mode == RuntimeMode::Partitioned {
            let lock = {
                let mut partitions =
                    self.partitions
                        .lock()
                        .map_err(|_| PluginHostError::Invocation {
                            plugin_id: self.plugin_id.clone(),
                            message: "partition lock registry is poisoned".to_owned(),
                        })?;
                partitions.retain(|_, lock| lock.strong_count() > 0);
                if let Some(lock) = partitions.get(&partition_key).and_then(Weak::upgrade) {
                    lock
                } else {
                    let lock = Arc::new(Mutex::new(()));
                    partitions.insert(partition_key, Arc::downgrade(&lock));
                    lock
                }
            };
            Some(lock.lock_owned().await)
        } else {
            None
        };
        let permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .map_err(|_| PluginHostError::Invocation {
                plugin_id: self.plugin_id.clone(),
                message: "invocation semaphore is closed".to_owned(),
            })?;
        Ok(InvocationGuard {
            _partition: partition,
            _permit: permit,
        })
    }

    async fn acquire_exclusive(&self) -> Result<OwnedSemaphorePermit, PluginHostError> {
        Arc::clone(&self.permits)
            .acquire_many_owned(self.max_concurrency)
            .await
            .map_err(|_| PluginHostError::Invocation {
                plugin_id: self.plugin_id.clone(),
                message: "invocation semaphore is closed".to_owned(),
            })
    }
}

struct InvocationGuard {
    _partition: Option<OwnedMutexGuard<()>>,
    _permit: OwnedSemaphorePermit,
}

fn event_partition_key(event: &Event, fallback: &str) -> String {
    match event {
        Event::Message(message) => message_target_partition_key(&message.target),
        _ => format!("event:{fallback}"),
    }
}

fn schedule_partition_key(task: &ScheduledTask) -> String {
    task.payload
        .get("target")
        .cloned()
        .and_then(|target| serde_json::from_value::<MessageTarget>(target).ok())
        .map_or_else(
            || format!("schedule:{}", task.task_id),
            |target| message_target_partition_key(&target),
        )
}

fn message_target_partition_key(target: &MessageTarget) -> String {
    match target {
        MessageTarget::Group { group_id } => format!("group:{group_id}"),
        MessageTarget::Private { user_id } => format!("private:{user_id}"),
        MessageTarget::Channel { channel_id } => format!("channel:{channel_id}"),
    }
}

struct RegisteredPlugin {
    plugin: Arc<dyn StaticPlugin>,
    manifest: PluginManifest,
    instance_id: String,
    config: StdMutex<BTreeMap<String, Value>>,
    granted_capabilities: BTreeSet<String>,
    execution: InvocationGate,
    state: StdMutex<LifecycleState>,
}

#[derive(Debug, Clone, Copy)]
struct LifecycleState {
    state: PluginInstanceState,
    generation: u64,
}

#[cfg(test)]
impl LifecycleState {
    const fn ready() -> Self {
        Self {
            state: PluginInstanceState::Ready,
            generation: 0,
        }
    }
}

impl RegisteredPlugin {
    fn config_snapshot(&self) -> Result<BTreeMap<String, Value>, PluginHostError> {
        self.config
            .lock()
            .map(|config| config.clone())
            .map_err(|_| PluginHostError::Invocation {
                plugin_id: self.manifest.id.to_string(),
                message: "plugin configuration lock is poisoned".to_owned(),
            })
    }

    fn replace_config(&self, config: BTreeMap<String, Value>) -> Result<(), PluginHostError> {
        *self
            .config
            .lock()
            .map_err(|_| PluginHostError::Invocation {
                plugin_id: self.manifest.id.to_string(),
                message: "plugin configuration lock is poisoned".to_owned(),
            })? = config;
        Ok(())
    }

    fn state(&self) -> Result<PluginInstanceState, PluginHostError> {
        self.state
            .lock()
            .map(|state| state.state)
            .map_err(|_| PluginHostError::Invocation {
                plugin_id: self.manifest.id.to_string(),
                message: "plugin lifecycle lock is poisoned".to_owned(),
            })
    }

    fn set_state(&self, state: PluginInstanceState) -> Result<(), PluginHostError> {
        self.state
            .lock()
            .map_err(|_| PluginHostError::Invocation {
                plugin_id: self.manifest.id.to_string(),
                message: "plugin lifecycle lock is poisoned".to_owned(),
            })?
            .state = state;
        Ok(())
    }

    fn begin_draining(&self) -> Result<(), PluginHostError> {
        let mut lifecycle = self.state.lock().map_err(|_| PluginHostError::Invocation {
            plugin_id: self.manifest.id.to_string(),
            message: "plugin lifecycle lock is poisoned".to_owned(),
        })?;
        lifecycle.state = PluginInstanceState::Draining;
        lifecycle.generation = lifecycle.generation.wrapping_add(1);
        Ok(())
    }

    fn ready_generation(&self) -> Result<Option<u64>, PluginHostError> {
        let lifecycle = self.state.lock().map_err(|_| PluginHostError::Invocation {
            plugin_id: self.manifest.id.to_string(),
            message: "plugin lifecycle lock is poisoned".to_owned(),
        })?;
        Ok((lifecycle.state == PluginInstanceState::Ready).then_some(lifecycle.generation))
    }

    fn is_ready_generation(&self, generation: u64) -> Result<bool, PluginHostError> {
        let lifecycle = self.state.lock().map_err(|_| PluginHostError::Invocation {
            plugin_id: self.manifest.id.to_string(),
            message: "plugin lifecycle lock is poisoned".to_owned(),
        })?;
        Ok(lifecycle.state == PluginInstanceState::Ready && lifecycle.generation == generation)
    }

    fn priority_for(&self, event_type: &str, scope: Option<&str>) -> Option<i32> {
        self.manifest
            .subscriptions
            .iter()
            .filter(|subscription| {
                subscription.event == event_type
                    && (subscription.scopes.is_empty()
                        || scope.is_some_and(|scope| subscription.scopes.contains(scope)))
            })
            .map(|subscription| subscription.priority)
            .min()
    }
}

struct InvocationQueries<'a> {
    config: &'a BTreeMap<String, Value>,
    state: &'a BTreeMap<String, StateValue>,
    granted_capabilities: &'a BTreeSet<String>,
    invocation_time_ms: i64,
}

impl HostQueries for InvocationQueries<'_> {
    fn config_get(&self, key: &str) -> Option<&Value> {
        self.config.get(key)
    }

    fn state_get(&self, key: &str) -> Option<&StateValue> {
        self.state.get(key)
    }

    fn state_scan(&self, prefix: &str, limit: usize) -> Vec<(&str, &StateValue)> {
        self.state
            .iter()
            .filter(|(key, _)| key.starts_with(prefix))
            .take(limit.min(MAX_STATE_SCAN_ENTRIES))
            .map(|(key, value)| (key.as_str(), value))
            .collect()
    }

    fn granted_capabilities(&self) -> &BTreeSet<String> {
        self.granted_capabilities
    }

    fn invocation_time_ms(&self) -> i64 {
        self.invocation_time_ms
    }
}

async fn supervised_shutdown(plugin: &RegisteredPlugin) -> Result<(), PluginHostError> {
    let invocation = AssertUnwindSafe(plugin.plugin.shutdown()).catch_unwind();
    timeout(plugin.manifest.timeout(), invocation)
        .await
        .map_err(|_| PluginHostError::Timeout {
            plugin_id: plugin.manifest.id.to_string(),
        })?
        .map_err(|_| PluginHostError::Panic {
            plugin_id: plugin.manifest.id.to_string(),
        })?
        .map_err(|error| PluginHostError::Invocation {
            plugin_id: plugin.manifest.id.to_string(),
            message: error.to_string(),
        })
}

async fn supervised_init(
    plugin: &RegisteredPlugin,
    config: BTreeMap<String, Value>,
) -> Result<(), PluginHostError> {
    let invocation = AssertUnwindSafe(plugin.plugin.init(InitContext {
        protocol_version: BPP_VERSION.to_owned(),
        plugin_id: plugin.manifest.id.clone(),
        instance_id: plugin.instance_id.clone(),
        granted_capabilities: plugin.granted_capabilities.clone(),
        config,
    }))
    .catch_unwind();
    timeout(plugin.manifest.timeout(), invocation)
        .await
        .map_err(|_| PluginHostError::Timeout {
            plugin_id: plugin.manifest.id.to_string(),
        })?
        .map_err(|_| PluginHostError::Panic {
            plugin_id: plugin.manifest.id.to_string(),
        })?
        .map_err(|error| PluginHostError::Init {
            plugin_id: plugin.manifest.id.to_string(),
            message: error.to_string(),
        })
}

async fn supervised_validate_config(
    plugin: &dyn StaticPlugin,
    manifest: &PluginManifest,
    config: &BTreeMap<String, Value>,
) -> Result<(), PluginHostError> {
    let invocation = AssertUnwindSafe(plugin.validate_config(config)).catch_unwind();
    timeout(manifest.timeout(), invocation)
        .await
        .map_err(|_| PluginHostError::Timeout {
            plugin_id: manifest.id.to_string(),
        })?
        .map_err(|_| PluginHostError::Panic {
            plugin_id: manifest.id.to_string(),
        })?
        .map_err(|error| PluginHostError::InvalidConfig {
            plugin_id: manifest.id.to_string(),
            message: error.to_string(),
        })
}

async fn supervised_migration(
    plugin: &RegisteredPlugin,
    from_version: u32,
    to_version: u32,
    state: &BTreeMap<String, StateValue>,
) -> Result<Vec<StateOp>, PluginHostError> {
    let invocation = AssertUnwindSafe(plugin.plugin.migrate_state(from_version, to_version, state))
        .catch_unwind();
    timeout(plugin.manifest.timeout(), invocation)
        .await
        .map_err(|_| PluginHostError::Timeout {
            plugin_id: plugin.manifest.id.to_string(),
        })?
        .map_err(|_| PluginHostError::Panic {
            plugin_id: plugin.manifest.id.to_string(),
        })?
        .map_err(|error| PluginHostError::Migration {
            plugin_id: plugin.manifest.id.to_string(),
            message: error.to_string(),
        })
}

async fn supervised_health(plugin: &RegisteredPlugin) -> Result<(), PluginHostError> {
    let invocation = AssertUnwindSafe(plugin.plugin.health()).catch_unwind();
    let health = timeout(plugin.manifest.timeout(), invocation)
        .await
        .map_err(|_| PluginHostError::Timeout {
            plugin_id: plugin.manifest.id.to_string(),
        })?
        .map_err(|_| PluginHostError::Panic {
            plugin_id: plugin.manifest.id.to_string(),
        })?
        .map_err(|error| PluginHostError::Invocation {
            plugin_id: plugin.manifest.id.to_string(),
            message: error.to_string(),
        })?;
    if health == HealthStatus::Healthy {
        Ok(())
    } else {
        Err(PluginHostError::Invocation {
            plugin_id: plugin.manifest.id.to_string(),
            message: "upgraded plugin reported degraded health".to_owned(),
        })
    }
}

const fn delivery_error_retryable(error: &PluginHostError) -> bool {
    matches!(
        error,
        PluginHostError::TransientInvocation { .. }
            | PluginHostError::Timeout { .. }
            | PluginHostError::Panic { .. }
    )
}

fn action_completion_depth(envelope: &PluginEventEnvelope) -> usize {
    envelope
        .extensions
        .iter()
        .find(|extension| extension.namespace == ACTION_COMPLETION_DEPTH_NAMESPACE)
        .and_then(|extension| std::str::from_utf8(&extension.data).ok())
        .and_then(|depth| depth.parse().ok())
        .unwrap_or(0)
}

fn delivery_retry_delay_ms(attempt: u32) -> u64 {
    let exponent = attempt.saturating_sub(1).min(10);
    DELIVERY_RETRY_BASE_MS.saturating_mul(1_u64 << exponent)
}

fn now_ms() -> i64 {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        sync::{Arc, Mutex as StdMutex},
    };

    use bot_core::MessageTarget;
    use builtin_plugins::PingPlugin;
    use plugin_api::{
        Disposition, ExtensionPayload, HandlerOutput, HostQueries, PluginCommand, PluginError,
        PluginEventEnvelope, PluginManifest, StaticPlugin,
    };
    use serde_json::{Value, json};

    use crate::{CommitOptions, OutboxOrigin, PluginStore};

    use super::{
        ACTION_COMPLETION_DEPTH_NAMESPACE, InvocationGate, LifecycleState, PluginHostError,
        RegisteredPlugin, StaticPluginHost, validate_config_schema, validate_output,
        validation::redact_sensitive_fields,
    };

    #[derive(Debug)]
    struct SchemaPlugin {
        manifest: PluginManifest,
        schema: Value,
    }

    #[derive(Debug)]
    struct CompletionRecoveryPlugin {
        manifest: PluginManifest,
        events: Arc<StdMutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl StaticPlugin for CompletionRecoveryPlugin {
        fn manifest(&self) -> &PluginManifest {
            &self.manifest
        }

        async fn on_event(
            &self,
            event: &PluginEventEnvelope,
            _queries: &dyn HostQueries,
        ) -> Result<HandlerOutput, PluginError> {
            self.events.lock().unwrap().push(event.event_type.clone());
            Ok(HandlerOutput::default())
        }
    }

    #[async_trait::async_trait]
    impl StaticPlugin for SchemaPlugin {
        fn manifest(&self) -> &PluginManifest {
            &self.manifest
        }

        fn config_schema(&self) -> Option<&Value> {
            Some(&self.schema)
        }

        async fn on_event(
            &self,
            _event: &PluginEventEnvelope,
            _queries: &dyn HostQueries,
        ) -> Result<HandlerOutput, PluginError> {
            Ok(HandlerOutput::default())
        }
    }

    fn schema_plugin(schema: Value) -> SchemaPlugin {
        SchemaPlugin {
            manifest: PingPlugin::default().manifest().clone(),
            schema,
        }
    }

    fn plugin_without_grants() -> RegisteredPlugin {
        let plugin = Arc::new(PingPlugin::default());
        let manifest = plugin.manifest().clone();
        let execution = InvocationGate::new(&manifest);
        RegisteredPlugin {
            manifest,
            plugin,
            instance_id: "dev.bkm.ping/test".to_owned(),
            config: StdMutex::new(BTreeMap::new()),
            granted_capabilities: BTreeSet::default(),
            execution,
            state: StdMutex::new(LifecycleState::ready()),
        }
    }

    #[test]
    fn config_schema_accepts_internal_references_and_rejects_invalid_config() {
        let plugin = schema_plugin(json!({
            "$defs": {
                "prefix": { "type": "string", "minLength": 1 }
            },
            "type": "object",
            "properties": {
                "prefix": { "$ref": "#/$defs/prefix" }
            },
            "required": ["prefix"]
        }));
        validate_config_schema(
            &plugin,
            "dev.bkm.schema/test",
            &BTreeMap::from([("prefix".to_owned(), json!("ok"))]),
        )
        .unwrap();

        let error = validate_config_schema(
            &plugin,
            "dev.bkm.schema/test",
            &BTreeMap::from([("prefix".to_owned(), json!(42))]),
        )
        .unwrap_err();
        assert!(matches!(error, PluginHostError::InvalidConfig { .. }));
    }

    #[test]
    fn config_schema_rejects_external_references_and_invalid_schemas() {
        let external = schema_plugin(json!({ "$ref": "https://example.com/schema.json" }));
        let error =
            validate_config_schema(&external, "dev.bkm.schema/test", &BTreeMap::new()).unwrap_err();
        assert!(matches!(
            error,
            PluginHostError::InvalidConfigSchema { message, .. }
                if message.contains("external schema reference")
        ));

        let invalid = schema_plugin(json!({ "type": 42 }));
        let error =
            validate_config_schema(&invalid, "dev.bkm.schema/test", &BTreeMap::new()).unwrap_err();
        assert!(matches!(error, PluginHostError::InvalidConfigSchema { .. }));
    }

    #[test]
    fn raw_event_extension_redacts_sensitive_fields_recursively() {
        let mut raw = json!({
            "authorization": "top-level",
            "nested": [{"client_secret": "nested"}],
            "content": "preserved"
        });
        redact_sensitive_fields(&mut raw);
        assert_eq!(raw["authorization"], "[REDACTED]");
        assert_eq!(raw["nested"][0]["client_secret"], "[REDACTED]");
        assert_eq!(raw["content"], "preserved");
    }

    fn reply_output(disposition: Disposition) -> HandlerOutput {
        HandlerOutput {
            disposition,
            state_ops: Vec::new(),
            commands: vec![PluginCommand {
                command_id: "reply".to_owned(),
                kind: "message.reply".to_owned(),
                idempotency_key: None,
                deadline_ms: None,
                payload: json!({"content":"pong"}),
            }],
            diagnostics: Vec::new(),
        }
    }

    #[test]
    fn rejects_ungranted_capability() {
        let error = validate_output(
            &plugin_without_grants(),
            &reply_output(Disposition::Continue),
        )
        .unwrap_err();
        assert!(error.to_string().contains("not granted"));
    }

    #[test]
    fn rejects_side_effects_with_ignore_disposition() {
        let error = validate_output(&plugin_without_grants(), &reply_output(Disposition::Ignore))
            .unwrap_err();
        assert!(error.to_string().contains("Ignore disposition"));
    }

    #[test]
    fn rejects_stop_without_permission() {
        let error = validate_output(
            &plugin_without_grants(),
            &HandlerOutput {
                disposition: Disposition::Stop,
                ..HandlerOutput::default()
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("stop_propagation"));
    }

    #[tokio::test]
    async fn committed_parent_recovers_unknown_completion_without_reinvocation() {
        let store = PluginStore::in_memory().unwrap();
        let instance_id = "dev.bkm.recovery/default";
        let source_event = PluginEventEnvelope {
            protocol_version: "1.0.0".to_owned(),
            event_id: "event-1".to_owned(),
            delivery_id: "delivery-1".to_owned(),
            invocation_id: "invocation-1".to_owned(),
            occurred_at_ms: None,
            received_at_ms: 1,
            adapter_id: "mock".to_owned(),
            event_type: "message.created".to_owned(),
            trace_id: None,
            payload: json!({"text":"/ping"}),
            extensions: Vec::new(),
        };
        let origin = OutboxOrigin {
            source_event_id: source_event.event_id.clone(),
            adapter_id: "mock".to_owned(),
            reply_target: Some(MessageTarget::Group {
                group_id: "group-1".to_owned(),
            }),
            source_message_id: Some("message-1".to_owned()),
        };
        store
            .begin_delivery(instance_id, &source_event, &origin, 1)
            .unwrap()
            .unwrap();
        store
            .commit(
                instance_id,
                &source_event.invocation_id,
                &[],
                &[PluginCommand {
                    command_id: "reply".to_owned(),
                    kind: "message.reply".to_owned(),
                    idempotency_key: None,
                    deadline_ms: None,
                    payload: json!({"content":"pong"}),
                }],
                CommitOptions::new(0)
                    .with_origin(&origin)
                    .with_delivery(&source_event.event_id, &source_event.delivery_id),
            )
            .unwrap();

        let events = Arc::new(StdMutex::new(Vec::new()));
        let plugin = Arc::new(CompletionRecoveryPlugin {
            manifest: PluginManifest::from_toml(
                r#"
                    manifest_version = 1
                    id = "dev.bkm.recovery"
                    version = "0.1.0"
                    protocol = ">=1.0,<2.0"

                    [metadata]
                    default_locale = "en"

                    [metadata.locales.en]
                    name = "Recovery"

                    [[subscriptions]]
                    id = "action-results"
                    event = "action.completed"
                "#,
            )
            .unwrap(),
            events: events.clone(),
        });
        let mut host = StaticPluginHost::new(store.clone());
        host.register_trusted(plugin, instance_id, BTreeMap::new())
            .await
            .unwrap();

        assert_eq!(events.lock().unwrap().as_slice(), ["action.completed"]);
        assert!(store.pending_commands(instance_id).unwrap().is_empty());
        assert!(
            store
                .begin_delivery(
                    instance_id,
                    &PluginEventEnvelope {
                        delivery_id: "delivery-2".to_owned(),
                        invocation_id: "invocation-2".to_owned(),
                        ..source_event
                    },
                    &origin,
                    2,
                )
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn recovered_completion_depth_limit_is_persistent() {
        let store = PluginStore::in_memory().unwrap();
        let instance_id = "dev.bkm.recovery/default";
        let envelope = PluginEventEnvelope {
            protocol_version: "1.0.0".to_owned(),
            event_id: "action/invocation/command".to_owned(),
            delivery_id: "delivery-1".to_owned(),
            invocation_id: "invocation-1".to_owned(),
            occurred_at_ms: None,
            received_at_ms: 1,
            adapter_id: "mock".to_owned(),
            event_type: "action.completed".to_owned(),
            trace_id: None,
            payload: json!({}),
            extensions: vec![ExtensionPayload {
                namespace: ACTION_COMPLETION_DEPTH_NAMESPACE.to_owned(),
                schema_version: "1.0.0".to_owned(),
                content_type: "text/plain".to_owned(),
                data: b"65".to_vec(),
            }],
        };
        store
            .enqueue_delivery(
                instance_id,
                &envelope,
                &origin_for_test(&envelope.event_id),
                1,
            )
            .unwrap();

        let events = Arc::new(StdMutex::new(Vec::new()));
        let plugin = Arc::new(CompletionRecoveryPlugin {
            manifest: PluginManifest::from_toml(
                r#"
                    manifest_version = 1
                    id = "dev.bkm.recovery"
                    version = "0.1.0"
                    protocol = ">=1.0,<2.0"

                    [metadata]
                    default_locale = "en"

                    [metadata.locales.en]
                    name = "Recovery"

                    [[subscriptions]]
                    id = "action-results"
                    event = "action.completed"
                "#,
            )
            .unwrap(),
            events: events.clone(),
        });
        let mut host = StaticPluginHost::new(store.clone());
        host.register_trusted(plugin, instance_id, BTreeMap::new())
            .await
            .unwrap();

        assert!(events.lock().unwrap().is_empty());
        assert!(store.circuit_open(instance_id).unwrap());
        assert_eq!(store.dead_letters(Some(instance_id)).unwrap().len(), 1);
    }

    fn origin_for_test(event_id: &str) -> OutboxOrigin {
        OutboxOrigin {
            source_event_id: event_id.to_owned(),
            adapter_id: "mock".to_owned(),
            reply_target: None,
            source_message_id: None,
        }
    }
}
