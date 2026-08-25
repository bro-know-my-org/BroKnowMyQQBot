//! Wasmtime Component Model backend for BPP plugins.

#![forbid(unsafe_code)]

mod conversion;

use conversion::{
    config_entries, guest_error, handler_output, runtime_error, state_entries, state_op, wit_event,
};

use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use crate::ValidatedPluginPackage;
use async_trait::async_trait;
use bot_core::{Event, MessageSegment, MessageTarget};
use plugin_api::{
    ActionCompleted, BrowserColorScheme, BrowserRun, BrowserScreenshotFormat, BrowserStep,
    BrowserViewport, BrowserWaitUntil, HandlerOutput, HealthStatus, HostQueries, HttpRequest,
    InitContext, MediaReply, MediaSend, PluginCommand, PluginDiagnostic, PluginError,
    PluginEventEnvelope, PluginManifest, PluginMessageTarget, ScheduleCancel, ScheduleCreate,
    ScheduleTriggered, StateOp, StateValue, StaticPlugin,
};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::Mutex;
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Config, Engine, ResourceLimiter, Store, StoreLimits, StoreLimitsBuilder};

wasmtime::component::bindgen!({
    path: "../plugin-api/wit",
    world: "plugin",
    async: true,
});

use bkm::plugin::{queries, types};

#[derive(Debug)]
struct StoreData {
    config: BTreeMap<String, Value>,
    state: BTreeMap<String, StateValue>,
    assets: BTreeMap<String, Vec<u8>>,
    granted_capabilities: BTreeSet<String>,
    invocation_time_ms: i64,
    limits: PluginResourceLimits,
}

#[derive(Debug)]
struct PluginResourceLimits {
    inner: StoreLimits,
    memory_bytes: usize,
}

#[derive(Debug, thiserror::Error)]
#[error("plugin linear memory limit exceeded")]
struct MemoryLimitExceeded;

impl ResourceLimiter for PluginResourceLimits {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> anyhow::Result<bool> {
        if desired > self.memory_bytes {
            return Err(MemoryLimitExceeded.into());
        }
        self.inner.memory_growing(current, desired, maximum)
    }

    fn memory_grow_failed(&mut self, error: anyhow::Error) -> anyhow::Result<()> {
        self.inner.memory_grow_failed(error)
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> anyhow::Result<bool> {
        self.inner.table_growing(current, desired, maximum)
    }

    fn table_grow_failed(&mut self, error: anyhow::Error) -> anyhow::Result<()> {
        self.inner.table_grow_failed(error)
    }

    fn instances(&self) -> usize {
        self.inner.instances()
    }

    fn tables(&self) -> usize {
        self.inner.tables()
    }

    fn memories(&self) -> usize {
        self.inner.memories()
    }
}

impl StoreData {
    fn encoded_json(value: &Value) -> types::EncodedValue {
        types::EncodedValue {
            schema_version: "1.0".to_owned(),
            content_type: "application/json".to_owned(),
            data: serde_json::to_vec(value).unwrap_or_else(|_| b"null".to_vec()),
        }
    }
}

impl types::Host for StoreData {}

impl queries::Host for StoreData {
    fn config_get(
        &mut self,
        key: String,
    ) -> impl Future<Output = Result<Option<types::EncodedValue>, types::HostError>> {
        std::future::ready(Ok(self.config.get(&key).map(Self::encoded_json)))
    }

    fn state_get(
        &mut self,
        key: String,
    ) -> impl Future<Output = Result<Option<types::StateValue>, types::HostError>> {
        std::future::ready(Ok(self.state.get(&key).map(|value| types::StateValue {
            value: value.value.clone(),
            revision: value.revision,
        })))
    }

    fn state_scan(
        &mut self,
        prefix: String,
        limit: u32,
    ) -> impl Future<Output = Result<Vec<types::StateEntry>, types::HostError>> {
        std::future::ready(Ok(self
            .state
            .range(prefix.clone()..)
            .take_while(|(key, _)| key.starts_with(&prefix))
            .take(usize::try_from(limit).unwrap_or(usize::MAX))
            .map(|(key, value)| types::StateEntry {
                key: key.clone(),
                value: value.value.clone(),
                revision: value.revision,
            })
            .collect()))
    }

    fn asset_get(
        &mut self,
        path: String,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, types::HostError>> {
        std::future::ready(Ok(self.assets.get(&path).cloned()))
    }

    fn granted_capabilities(&mut self) -> impl Future<Output = Vec<String>> {
        std::future::ready(self.granted_capabilities.iter().cloned().collect())
    }

    fn invocation_time_ms(&mut self) -> impl Future<Output = i64> {
        std::future::ready(self.invocation_time_ms)
    }
}

struct WasmInstance {
    store: Store<StoreData>,
    bindings: Plugin,
}

pub struct WasmPlugin {
    manifest: PluginManifest,
    config_schema: Option<Value>,
    engine: Engine,
    fuel: u64,
    timeout: Duration,
    instance: Mutex<WasmInstance>,
}

impl std::fmt::Debug for WasmPlugin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WasmPlugin")
            .field("manifest", &self.manifest)
            .field("has_config_schema", &self.config_schema.is_some())
            .field("fuel", &self.fuel)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl WasmPlugin {
    pub async fn from_package(package: ValidatedPluginPackage) -> Result<Self, WasmPluginError> {
        let (manifest, config_schema, assets, engine, component) =
            tokio::task::spawn_blocking(move || {
                let manifest = package.manifest().clone();
                let config_schema = package.config_schema().cloned();
                let assets = package
                    .files()
                    .filter(|(path, _)| path.starts_with("assets/"))
                    .map(|(path, data)| (path.to_owned(), data.to_vec()))
                    .collect();
                let mut config = Config::new();
                config.wasm_component_model(true);
                config.async_support(true);
                config.consume_fuel(true);
                config.epoch_interruption(true);
                let engine = Engine::new(&config).map_err(WasmPluginError::Wasmtime)?;
                let component = Component::new(&engine, package.component())
                    .map_err(WasmPluginError::Wasmtime)?;
                Ok::<_, WasmPluginError>((manifest, config_schema, assets, engine, component))
            })
            .await
            .map_err(WasmPluginError::PreparationTask)??;
        let mut linker = Linker::new(&engine);
        Plugin::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
            .map_err(WasmPluginError::Wasmtime)?;
        let memory_bytes = usize::try_from(manifest.runtime.memory_mb)
            .unwrap_or(usize::MAX)
            .saturating_mul(1024 * 1024);
        let limits = StoreLimitsBuilder::new()
            .memory_size(memory_bytes)
            .instances(16)
            .tables(16)
            .trap_on_grow_failure(true)
            .build();
        let mut store = Store::new(
            &engine,
            StoreData {
                config: BTreeMap::new(),
                state: BTreeMap::new(),
                assets,
                granted_capabilities: BTreeSet::new(),
                invocation_time_ms: 0,
                limits: PluginResourceLimits {
                    inner: limits,
                    memory_bytes,
                },
            },
        );
        store.limiter(|state| &mut state.limits);
        store.set_epoch_deadline(u64::MAX);
        store
            .set_fuel(manifest.runtime.fuel)
            .map_err(WasmPluginError::Wasmtime)?;
        store
            .fuel_async_yield_interval(Some(10_000))
            .map_err(WasmPluginError::Wasmtime)?;
        let bindings = Plugin::instantiate_async(&mut store, &component, &linker)
            .await
            .map_err(WasmPluginError::Wasmtime)?;
        Ok(Self {
            fuel: manifest.runtime.fuel,
            timeout: manifest.timeout(),
            manifest,
            config_schema,
            engine,
            instance: Mutex::new(WasmInstance { store, bindings }),
        })
    }

    async fn lock_instance(&self) -> tokio::sync::MutexGuard<'_, WasmInstance> {
        self.instance.lock().await
    }

    fn prepare_invocation(
        &self,
        store: &mut Store<StoreData>,
    ) -> Result<EpochDeadline, PluginError> {
        reset_fuel(store, self.fuel)?;
        store.set_epoch_deadline(1);
        let engine = self.engine.clone();
        let timeout = self.timeout;
        let timer = tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            engine.increment_epoch();
        });
        Ok(EpochDeadline { timer: Some(timer) })
    }
}

struct EpochDeadline {
    timer: Option<tokio::task::JoinHandle<()>>,
}

impl EpochDeadline {
    async fn cancel(mut self) {
        if let Some(timer) = self.timer.take() {
            timer.abort();
            let _ = timer.await;
        }
    }
}

impl Drop for EpochDeadline {
    fn drop(&mut self) {
        if let Some(timer) = &self.timer {
            timer.abort();
        }
    }
}

#[derive(Debug, Error)]
pub enum WasmPluginError {
    #[error("Wasmtime component operation failed")]
    Wasmtime(#[source] anyhow::Error),
    #[error("Wasm plugin preparation task failed")]
    PreparationTask(#[source] tokio::task::JoinError),
}

#[async_trait]
impl StaticPlugin for WasmPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn config_schema(&self) -> Option<&Value> {
        self.config_schema.as_ref()
    }

    async fn validate_config(&self, config: &BTreeMap<String, Value>) -> Result<(), PluginError> {
        let mut instance = self.lock_instance().await;
        let epoch_deadline = self.prepare_invocation(&mut instance.store)?;
        instance.store.data_mut().config.clone_from(config);
        let config = config_entries(config);
        let WasmInstance { store, bindings } = &mut *instance;
        let result = bindings
            .bkm_plugin_lifecycle()
            .call_validate_config(store, &config)
            .await
            .map_err(runtime_error)
            .and_then(|result| result.map_err(guest_error));
        epoch_deadline.cancel().await;
        result
    }

    async fn init(&self, context: InitContext) -> Result<(), PluginError> {
        let mut instance = self.lock_instance().await;
        let epoch_deadline = self.prepare_invocation(&mut instance.store)?;
        instance.store.data_mut().config = context.config.clone();
        instance
            .store
            .data_mut()
            .granted_capabilities
            .clone_from(&context.granted_capabilities);
        let context = types::InitContext {
            protocol_version: context.protocol_version,
            plugin_id: context.plugin_id.to_string(),
            instance_id: context.instance_id,
            granted_capabilities: context.granted_capabilities.into_iter().collect(),
            config: config_entries(&context.config),
        };
        let WasmInstance { store, bindings } = &mut *instance;
        let result = bindings
            .bkm_plugin_lifecycle()
            .call_init(store, &context)
            .await
            .map_err(runtime_error)
            .and_then(|result| result.map_err(guest_error));
        epoch_deadline.cancel().await;
        result
    }

    async fn on_event(
        &self,
        event: &PluginEventEnvelope,
        queries: &dyn HostQueries,
    ) -> Result<HandlerOutput, PluginError> {
        let event = wit_event(event)?;
        let state = queries
            .state_scan("", usize::MAX)
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value.clone()))
            .collect();
        let mut instance = self.lock_instance().await;
        let epoch_deadline = self.prepare_invocation(&mut instance.store)?;
        instance.store.data_mut().state = state;
        instance
            .store
            .data_mut()
            .granted_capabilities
            .clone_from(queries.granted_capabilities());
        instance.store.data_mut().invocation_time_ms = queries.invocation_time_ms();
        let WasmInstance { store, bindings } = &mut *instance;
        let output = bindings
            .bkm_plugin_handler()
            .call_on_event(store, &event)
            .await
            .map_err(runtime_error)
            .and_then(|result| result.map_err(guest_error));
        epoch_deadline.cancel().await;
        handler_output(output?)
    }

    async fn health(&self) -> Result<HealthStatus, PluginError> {
        let mut instance = self.lock_instance().await;
        let epoch_deadline = self.prepare_invocation(&mut instance.store)?;
        let WasmInstance { store, bindings } = &mut *instance;
        let health = bindings
            .bkm_plugin_lifecycle()
            .call_health(store)
            .await
            .map_err(runtime_error)
            .and_then(|result| result.map_err(guest_error));
        epoch_deadline.cancel().await;
        let health = health?;
        Ok(match health {
            types::HealthStatus::Healthy => HealthStatus::Healthy,
            types::HealthStatus::Degraded => HealthStatus::Degraded,
        })
    }

    async fn migrate_state(
        &self,
        from_version: u32,
        to_version: u32,
        state: &BTreeMap<String, StateValue>,
    ) -> Result<Vec<StateOp>, PluginError> {
        let mut instance = self.lock_instance().await;
        let epoch_deadline = self.prepare_invocation(&mut instance.store)?;
        instance.store.data_mut().state.clone_from(state);
        let state = state_entries(state);
        let WasmInstance { store, bindings } = &mut *instance;
        let output = bindings
            .bkm_plugin_lifecycle()
            .call_migrate_state(store, from_version, to_version, &state)
            .await
            .map_err(runtime_error)
            .and_then(|result| result.map_err(guest_error));
        epoch_deadline.cancel().await;
        let output = output?;
        Ok(output.state_ops.into_iter().map(state_op).collect())
    }

    async fn shutdown(&self) -> Result<(), PluginError> {
        let mut instance = self.lock_instance().await;
        let epoch_deadline = self.prepare_invocation(&mut instance.store)?;
        let WasmInstance { store, bindings } = &mut *instance;
        let result = bindings
            .bkm_plugin_lifecycle()
            .call_shutdown(store)
            .await
            .map_err(runtime_error)
            .and_then(|result| result.map_err(guest_error));
        epoch_deadline.cancel().await;
        result
    }
}

fn reset_fuel(store: &mut Store<StoreData>, fuel: u64) -> Result<(), PluginError> {
    store.set_fuel(fuel).map_err(runtime_error)
}
