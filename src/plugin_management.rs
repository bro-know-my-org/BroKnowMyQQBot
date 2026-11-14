//! Shared management operations for the CLI and authenticated Web interface.

pub(crate) mod marketplace;
pub(crate) mod marketplace_install;
mod operations;

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex as StdMutex, RwLock},
};

use crate::plugin_configuration::{
    self, ConfigurationError, checked_package, checked_plugin, require_grants,
};
use async_trait::async_trait;
use bot_core::{Context, Event, EventHandler, HandlerError};
use plugin_host::{
    PluginHostError, PluginInstallation, PluginInstanceSnapshot, PluginInstanceState, PluginStore,
    StaticPluginHost, StoreError, validate_plugin_config,
};
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;
use tokio::sync::Mutex;
use tokio::{
    sync::{Semaphore, oneshot},
    task::JoinSet,
};

#[derive(Debug, Error)]
pub(crate) enum ManagementError {
    #[error("市场暂不可用，请稍后重试或检查配置的索引")]
    MarketplaceUnavailable,
    #[error("市场条目已改变，请刷新详情并重新确认")]
    StaleMarketplace,
    #[error("市场请求无效，请重新选择插件")]
    InvalidMarketRequest,
    #[error("插件操作无效，请重新确认操作与当前实例")]
    InvalidOperation,
    #[error("插件不存在，或由声明式文件管理")]
    NotManaged,
    #[error("插件记录已改变，请刷新后重试")]
    Conflict,
    #[error("插件包校验失败，请检查安装记录或重新安装")]
    InvalidPackage,
    #[error("配置校验失败，请检查字段类型、必填项和插件配置约束")]
    InvalidConfig,
    #[error("插件缺少必要授权，请通过安装命令显式授权")]
    MissingGrants,
    #[error("插件包或授权已改变，需要重启 Bot 后生效")]
    RestartRequired,
    #[error("插件已故障禁用，需要先执行 recover 并重启 Bot")]
    Disabled,
    #[error("插件生命周期操作失败，已保留保存的配置，请检查运行状态")]
    Lifecycle,
    #[error("插件管理存储不可用")]
    Store,
    #[error("命令前缀无效，请使用 1 至 32 个字符且不含空白或控制字符的字符串")]
    InvalidSettings,
    #[error("系统设置无效，请检查适配器组合、监听地址、过滤器与容量限制")]
    InvalidSystemSettings,
    #[error("Bot 配置文件不可写或校验失败，请检查文件权限与配置内容")]
    SettingsUnavailable,
    #[error("插件管理操作繁忙或正在关闭，请稍后重试")]
    Busy,
}

impl From<StoreError> for ManagementError {
    fn from(error: StoreError) -> Self {
        if matches!(error, StoreError::InstallationConflict) {
            Self::Conflict
        } else {
            Self::Store
        }
    }
}

impl From<ConfigurationError> for ManagementError {
    fn from(error: ConfigurationError) -> Self {
        match error {
            ConfigurationError::NotManaged => Self::NotManaged,
            ConfigurationError::Conflict => Self::Conflict,
            ConfigurationError::InvalidPackage => Self::InvalidPackage,
            ConfigurationError::InvalidConfig => Self::InvalidConfig,
            ConfigurationError::MissingGrants => Self::MissingGrants,
            ConfigurationError::Store => Self::Store,
        }
    }
}

#[derive(Debug, Serialize)]
#[allow(clippy::struct_excessive_bools)] // Independent persisted, lifecycle and UI flags in the API view.
pub(crate) struct PluginView {
    instance_id: String,
    plugin_id: String,
    name: String,
    description: String,
    version: String,
    saved_version: Option<String>,
    loaded_version: Option<String>,
    restart_required: bool,
    source: String,
    trust_level: String,
    signature_status: String,
    enabled: bool,
    runtime_state: &'static str,
    editable: bool,
    config: BTreeMap<String, Value>,
    schema: Option<Value>,
    requested_permissions: Vec<String>,
    granted_permissions: Vec<String>,
    updated_at_ms: i64,
    pending: bool,
    error: Option<String>,
}

type ShutdownTask = tokio::task::JoinHandle<Result<(), PluginHostError>>;

#[derive(Clone)]
pub(crate) struct PluginManager {
    store: PluginStore,
    runtime: Arc<RwLock<RuntimeState>>,
    writes: Arc<Mutex<()>>,
    operations: Arc<StdMutex<Operations>>,
    operation_slots: Arc<Semaphore>,
    shutdown_worker: Arc<Mutex<Option<ShutdownTask>>>,
    file_instances: Arc<BTreeSet<String>>,
    marketplace: Option<marketplace::MarketplaceContext>,
}

#[derive(Default)]
struct Operations {
    tasks: JoinSet<()>,
    closing: bool,
}

#[derive(Clone)]
struct RuntimeState {
    host: StaticPluginHost,
    packages: BTreeMap<String, String>,
    restart_required: BTreeSet<String>,
    retained_packages: BTreeSet<std::path::PathBuf>,
}

impl PluginManager {
    pub(crate) fn command_prefix(&self) -> Result<String, ManagementError> {
        Ok(self.snapshot()?.host.command_prefix())
    }

    pub(crate) fn set_command_prefix(&self, prefix: &str) -> Result<(), ManagementError> {
        self.snapshot()?
            .host
            .set_command_prefix(prefix)
            .map_err(|_| ManagementError::InvalidSettings)
    }

    pub(crate) fn new(
        host: StaticPluginHost,
        store: PluginStore,
        packages: BTreeMap<String, String>,
        file_instances: BTreeSet<String>,
    ) -> Self {
        Self {
            marketplace: None,
            store,
            runtime: Arc::new(RwLock::new(RuntimeState {
                host,
                packages,
                restart_required: BTreeSet::new(),
                retained_packages: BTreeSet::new(),
            })),
            writes: Arc::new(Mutex::new(())),
            operations: Arc::new(StdMutex::new(Operations::default())),
            operation_slots: Arc::new(Semaphore::new(4)),
            shutdown_worker: Arc::new(Mutex::new(None)),
            file_instances: Arc::new(file_instances),
        }
    }

    fn snapshot(&self) -> Result<RuntimeState, ManagementError> {
        self.runtime
            .read()
            .map(|state| state.clone())
            .map_err(|_| ManagementError::Store)
    }

    pub(crate) async fn shutdown(&self) -> Result<(), PluginHostError> {
        // Await the retained handle by reference. Cancelling this caller releases
        // the lock, while the supervisor keeps ownership of operations and Host.
        let mut worker = self.shutdown_worker.lock().await;
        if worker.is_none() {
            let manager = self.clone();
            let runtime = Arc::clone(&self.runtime);
            let mut operations = self
                .operations
                .lock()
                .map_err(|_| PluginHostError::Store(StoreError::Poisoned))?;
            operations.closing = true;
            let mut tasks = std::mem::take(&mut operations.tasks);
            *worker = Some(tokio::spawn(async move {
                let mut failed = false;
                while let Some(result) = tasks.join_next().await {
                    failed |= result.is_err();
                }
                let host = runtime
                    .read()
                    .map_err(|_| PluginHostError::Store(StoreError::Poisoned))?
                    .host
                    .clone();
                host.shutdown().await?;
                manager
                    .cleanup_retained_packages()
                    .await
                    .map_err(|_| PluginHostError::Store(StoreError::Poisoned))?;
                if failed {
                    return Err(PluginHostError::Store(StoreError::Poisoned));
                }
                Ok(())
            }));
        }
        let result = worker
            .as_mut()
            .expect("shutdown worker was installed")
            .await;
        *worker = None;
        result.map_err(|error| {
            tracing::error!(%error, "plugin shutdown supervisor failed");
            PluginHostError::Store(StoreError::Poisoned)
        })?
    }

    pub(crate) async fn list(&self) -> Result<Vec<PluginView>, ManagementError> {
        let runtime = self.snapshot()?;
        let snapshots = runtime
            .host
            .instance_snapshots()
            .map_err(|_| ManagementError::Store)?;
        let mut views = BTreeMap::new();
        for snapshot in &snapshots {
            views.insert(snapshot.instance_id.clone(), readonly_view(snapshot)?);
        }
        let store = self.store.clone();
        let installations = tokio::task::spawn_blocking(move || store.installations())
            .await
            .map_err(|_| ManagementError::Store)??;
        for installation in installations {
            let snapshot = snapshots
                .iter()
                .find(|item| item.instance_id == installation.instance_id);
            let view = managed_view(&installation, snapshot, &runtime, &self.file_instances).await;
            views.insert(installation.instance_id, view);
        }
        Ok(views.into_values().collect())
    }

    pub(crate) async fn view(&self, instance_id: &str) -> Result<PluginView, ManagementError> {
        let runtime = self.snapshot()?;
        let snapshot = runtime
            .host
            .instance_snapshots()
            .map_err(|_| ManagementError::Store)?
            .into_iter()
            .find(|item| item.instance_id == instance_id);
        let store = self.store.clone();
        let id = instance_id.to_owned();
        let installation = tokio::task::spawn_blocking(move || store.installation(&id))
            .await
            .map_err(|_| ManagementError::Store)??;
        if let Some(installation) = installation {
            Ok(managed_view(
                &installation,
                snapshot.as_ref(),
                &runtime,
                &self.file_instances,
            )
            .await)
        } else {
            readonly_view(&snapshot.ok_or(ManagementError::NotManaged)?)
        }
    }

    async fn current(
        &self,
        instance_id: &str,
        expected: i64,
    ) -> Result<PluginInstallation, ManagementError> {
        if crate::plugins::is_reserved_bundled_instance_id(instance_id)
            || self.file_instances.contains(instance_id)
        {
            return Err(ManagementError::NotManaged);
        }
        let runtime = self.snapshot()?;
        if runtime.host.instance_manifest(instance_id).is_some()
            && !runtime.packages.contains_key(instance_id)
        {
            return Err(ManagementError::NotManaged);
        }
        Ok(plugin_configuration::current(&self.store, instance_id, expected).await?)
    }

    pub(crate) async fn save_config(
        &self,
        instance_id: &str,
        expected: i64,
        config: BTreeMap<String, Value>,
    ) -> Result<(), ManagementError> {
        let _write = self.writes.lock().await;
        self.current(instance_id, expected).await?;
        plugin_configuration::save_config(&self.store, instance_id, expected, config).await?;
        Ok(())
    }

    pub(crate) async fn save_enabled(
        &self,
        instance_id: &str,
        expected: i64,
        enabled: bool,
    ) -> Result<(), ManagementError> {
        let _write = self.writes.lock().await;
        self.current(instance_id, expected).await?;
        plugin_configuration::save_enabled(&self.store, instance_id, expected, enabled).await?;
        Ok(())
    }

    pub(crate) async fn apply(
        &self,
        instance_id: &str,
        expected: i64,
    ) -> Result<(), ManagementError> {
        let manager = self.clone();
        let instance_id = instance_id.to_owned();
        self.operation(async move { manager.apply_inner(&instance_id, expected).await })
            .await
    }

    async fn operation<T: Send + 'static>(
        &self,
        task: impl std::future::Future<Output = Result<T, ManagementError>> + Send + 'static,
    ) -> Result<T, ManagementError> {
        let permit = self
            .operation_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| ManagementError::Busy)?;
        let (send, receive) = oneshot::channel();
        {
            let mut operations = self.operations.lock().map_err(|_| ManagementError::Store)?;
            if operations.closing {
                return Err(ManagementError::Busy);
            }
            while let Some(result) = operations.tasks.try_join_next() {
                if result.is_err() {
                    tracing::warn!("plugin management operation task failed");
                }
            }
            operations.tasks.spawn(async move {
                let _permit = permit;
                let result = task.await;
                let _ = send.send(result);
            });
        }
        receive.await.map_err(|_| ManagementError::Store)?
    }

    async fn apply_inner(&self, instance_id: &str, expected: i64) -> Result<(), ManagementError> {
        let _write = self.writes.lock().await;
        let installation = self.current(instance_id, expected).await?;
        let mut runtime = self.snapshot()?;
        let existing = runtime
            .host
            .instance_snapshots()
            .map_err(|_| ManagementError::Store)?
            .into_iter()
            .find(|item| item.instance_id == instance_id);
        if let Some(existing) = &existing {
            if !runtime.packages.contains_key(instance_id) {
                return Err(ManagementError::NotManaged);
            }
            if !installation.enabled {
                return runtime
                    .host
                    .set_instance_enabled(instance_id, false)
                    .await
                    .map_err(|_| ManagementError::Lifecycle);
            }
            if runtime.packages.get(instance_id) != Some(&installation.package_sha256)
                || existing.granted_capabilities
                    != installation.granted_permissions.iter().cloned().collect()
            {
                return Err(ManagementError::RestartRequired);
            }
            if existing.state == PluginInstanceState::Disabled {
                return Err(if runtime.restart_required.contains(instance_id) {
                    ManagementError::RestartRequired
                } else {
                    ManagementError::Disabled
                });
            }
        } else if !installation.enabled {
            return Ok(());
        }
        if runtime.restart_required.contains(instance_id) {
            return Err(ManagementError::RestartRequired);
        }
        require_grants(&installation)?;
        let plugin = checked_plugin(&installation).await?;
        validate_plugin_config(plugin.as_ref(), &installation.config)
            .await
            .map_err(|_| ManagementError::InvalidConfig)?;
        // The record may have been changed by another CLI process during validation.
        self.current(instance_id, expected).await?;
        if let Some(existing) = existing {
            if existing.state == PluginInstanceState::Stopped {
                runtime
                    .host
                    .configure_stopped_instance(instance_id, installation.config)
                    .await
                    .map_err(|_| ManagementError::Lifecycle)?;
                runtime
                    .host
                    .set_instance_enabled(instance_id, true)
                    .await
                    .map_err(|_| ManagementError::Lifecycle)?;
            } else if existing.config != installation.config {
                runtime
                    .host
                    .update_config(instance_id, installation.config)
                    .await
                    .map_err(|_| ManagementError::Lifecycle)?;
            }
        } else {
            runtime
                .host
                .register(
                    plugin,
                    instance_id,
                    installation.config,
                    installation.granted_permissions.into_iter().collect(),
                )
                .await
                .map_err(|_| ManagementError::Lifecycle)?;
            runtime
                .packages
                .insert(instance_id.to_owned(), installation.package_sha256);
            *self.runtime.write().map_err(|_| ManagementError::Store)? = runtime;
        }
        Ok(())
    }
}

#[async_trait]
impl EventHandler for PluginManager {
    fn name(&self) -> &'static str {
        "managed-plugin-host"
    }

    async fn handle(&self, context: Context, event: &Event) -> Result<(), HandlerError> {
        let host = self
            .runtime
            .read()
            .map(|runtime| runtime.host.clone())
            .map_err(|_| HandlerError::Failed(ManagementError::Store.to_string()))?;
        host.handle(context, event).await
    }
}

fn state_name(state: PluginInstanceState) -> &'static str {
    match state {
        PluginInstanceState::Ready => "ready",
        PluginInstanceState::Draining => "draining",
        PluginInstanceState::Disabled => "disabled",
        PluginInstanceState::Stopped => "stopped",
    }
}

fn readonly_view(snapshot: &PluginInstanceSnapshot) -> Result<PluginView, ManagementError> {
    let metadata = snapshot
        .manifest
        .metadata
        .resolve("zh-CN")
        .map_err(|_| ManagementError::Store)?;
    Ok(PluginView {
        instance_id: snapshot.instance_id.clone(),
        plugin_id: snapshot.manifest.id.to_string(),
        name: metadata.name.clone(),
        description: metadata.description.clone(),
        version: snapshot.manifest.version.clone(),
        saved_version: None,
        loaded_version: Some(snapshot.manifest.version.clone()),
        restart_required: false,
        source: "声明式配置 / 随包插件".to_owned(),
        trust_level: "file-managed".to_owned(),
        signature_status: "n/a".to_owned(),
        enabled: snapshot.state == PluginInstanceState::Ready,
        runtime_state: state_name(snapshot.state),
        editable: false,
        config: snapshot.config.clone(),
        schema: snapshot.schema.clone(),
        requested_permissions: snapshot
            .manifest
            .requested_capabilities()
            .into_iter()
            .collect(),
        granted_permissions: snapshot.granted_capabilities.iter().cloned().collect(),
        updated_at_ms: 0,
        pending: false,
        error: None,
    })
}

async fn managed_view(
    installation: &PluginInstallation,
    snapshot: Option<&PluginInstanceSnapshot>,
    runtime: &RuntimeState,
    file_instances: &BTreeSet<String>,
) -> PluginView {
    let mut view = installed_view(installation, snapshot);
    view.restart_required = restart_required(installation, snapshot, runtime);
    view.pending |= view.restart_required;
    match checked_package(installation).await {
        Ok(package) => view.schema = package.config_schema().cloned(),
        Err(error) => view.error = Some(error.to_string()),
    }
    if crate::plugins::is_reserved_bundled_instance_id(&installation.instance_id)
        || file_instances.contains(&installation.instance_id)
        || (snapshot.is_some() && !runtime.packages.contains_key(&installation.instance_id))
    {
        view.editable = false;
        view.error = Some("实例 ID 与文件管理的插件冲突，请移除重复安装记录".to_owned());
    }
    view
}

fn restart_required(
    installation: &PluginInstallation,
    snapshot: Option<&PluginInstanceSnapshot>,
    runtime: &RuntimeState,
) -> bool {
    runtime.restart_required.contains(&installation.instance_id)
        || snapshot.is_some_and(|snapshot| {
            runtime.packages.get(&installation.instance_id) != Some(&installation.package_sha256)
                || snapshot.granted_capabilities
                    != installation.granted_permissions.iter().cloned().collect()
        })
}

fn installed_view(
    item: &PluginInstallation,
    snapshot: Option<&PluginInstanceSnapshot>,
) -> PluginView {
    let (name, description, error) = item.metadata.resolve("zh-CN").map_or_else(
        |_| {
            (
                item.plugin_id.clone(),
                String::new(),
                Some("插件元数据无效，请检查安装记录或重新安装".to_owned()),
            )
        },
        |metadata| (metadata.name.clone(), metadata.description.clone(), None),
    );
    let ready = snapshot.is_some_and(|item| item.state == PluginInstanceState::Ready);
    let pending = ready != item.enabled
        || (item.enabled
            && snapshot.is_some_and(|snapshot| {
                snapshot.config != item.config
                    || snapshot.granted_capabilities
                        != item.granted_permissions.iter().cloned().collect()
            }));
    PluginView {
        instance_id: item.instance_id.clone(),
        plugin_id: item.plugin_id.clone(),
        name,
        description,
        version: item.version.clone(),
        saved_version: Some(item.version.clone()),
        loaded_version: snapshot.map(|snapshot| snapshot.manifest.version.clone()),
        restart_required: false,
        source: item.source.clone(),
        trust_level: item.trust_level.clone(),
        signature_status: item.signature_status.clone(),
        enabled: item.enabled,
        runtime_state: snapshot.map_or("not_loaded", |item| state_name(item.state)),
        editable: true,
        config: item.config.clone(),
        schema: None,
        requested_permissions: item.requested_permissions.clone(),
        granted_permissions: item.granted_permissions.clone(),
        updated_at_ms: item.updated_at_ms,
        pending,
        error,
    }
}

#[cfg(test)]
mod tests;
