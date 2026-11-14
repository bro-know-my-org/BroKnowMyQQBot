//! Read-only management snapshots and supervised instance activation.

use super::{
    PluginHostError, PluginInstanceState, RegisteredPlugin, StaticPluginHost,
    lifecycle_error_is_poisoning, supervised_activate, supervised_shutdown, validate_plugin_config,
};
use plugin_api::PluginManifest;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

/// Roll back an unstarted transition, but never reuse a guest whose lifecycle
/// future was dropped while running. The caller must hold the lifecycle lock.
pub(super) struct LifecycleTransition<'a> {
    host: StaticPluginHost,
    plugin: &'a Arc<RegisteredPlugin>,
    fallback: Option<PluginInstanceState>,
}

impl<'a> LifecycleTransition<'a> {
    pub(super) fn begin(
        host: &StaticPluginHost,
        plugin: &'a Arc<RegisteredPlugin>,
    ) -> Result<Self, PluginHostError> {
        let previous = plugin.state()?;
        plugin.begin_draining()?;
        Ok(Self {
            host: host.clone(),
            plugin,
            fallback: Some(previous),
        })
    }

    pub(super) fn hooks_started(&mut self) {
        self.fallback = Some(PluginInstanceState::Disabled);
    }

    pub(super) fn finish(mut self) {
        self.fallback = None;
    }
}

impl Drop for LifecycleTransition<'_> {
    fn drop(&mut self) {
        if let Some(fallback) = self.fallback {
            // In-flight events can poison a guest while its transition waits.
            // Keep the incremented generation even when restoring Ready.
            if self.plugin.state().ok() != Some(PluginInstanceState::Disabled) {
                if let Err(error) = self.plugin.set_state(fallback) {
                    tracing::error!(instance_id = self.plugin.instance_id, %error,
                        "failed to finalize cancelled plugin lifecycle");
                } else if fallback == PluginInstanceState::Ready {
                    // Pending work may already have retired in Draining. Late
                    // requeues are covered by the worker's atomic retirement.
                    if let Err(error) = self.host.resume_pending_schedules(self.plugin) {
                        let disable_error =
                            self.plugin.set_state(PluginInstanceState::Disabled).err();
                        tracing::error!(instance_id = self.plugin.instance_id, %error, ?disable_error,
                            "failed to resume schedules after cancelled plugin lifecycle");
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct PluginInstanceSnapshot {
    pub instance_id: String,
    pub manifest: PluginManifest,
    pub state: PluginInstanceState,
    pub config: BTreeMap<String, Value>,
    pub schema: Option<Value>,
    pub granted_capabilities: BTreeSet<String>,
}

impl StaticPluginHost {
    /// Detach only after draining all in-flight work. Old Host snapshots share
    /// the terminal state, so queued dispatches cannot re-enter the guest.
    pub async fn unregister_instance(&mut self, instance_id: &str) -> Result<(), PluginHostError> {
        self.retire_instance(instance_id).await?;
        self.plugins
            .retain(|plugin| plugin.instance_id != instance_id);
        self.instances.remove(instance_id);
        self.rebuild_command_names();
        Ok(())
    }

    /// Stop all work and irreversibly retire this guest, retaining its snapshot.
    /// Recovery must instantiate a fresh guest after a process restart.
    pub async fn retire_instance(&self, instance_id: &str) -> Result<(), PluginHostError> {
        let plugin = self
            .plugins
            .iter()
            .find(|plugin| plugin.instance_id == instance_id)
            .cloned()
            .ok_or_else(|| PluginHostError::Invocation {
                plugin_id: instance_id.to_owned(),
                message: "plugin instance is not registered".to_owned(),
            })?;
        let _lifecycle = plugin.lifecycle.lock().await;
        self.set_registered_enabled(&plugin, false).await?;
        // Irreversible while holding the same lifecycle lock: an older Host
        // clone, including a queued enable, can never revive this guest.
        plugin.set_state(PluginInstanceState::Disabled)?;
        Ok(())
    }

    pub async fn configure_stopped_instance(
        &self,
        instance_id: &str,
        config: BTreeMap<String, Value>,
    ) -> Result<(), PluginHostError> {
        let plugin = self
            .plugins
            .iter()
            .find(|plugin| plugin.instance_id == instance_id)
            .ok_or_else(|| PluginHostError::Invocation {
                plugin_id: instance_id.to_owned(),
                message: "plugin instance is not registered".to_owned(),
            })?;
        let _lifecycle = plugin.lifecycle.lock().await;
        let _execution = plugin.execution.acquire_exclusive().await?;
        if plugin.state()? != PluginInstanceState::Stopped {
            return Err(PluginHostError::Invocation {
                plugin_id: instance_id.to_owned(),
                message: "plugin instance is not stopped".to_owned(),
            });
        }
        let mut transition = LifecycleTransition::begin(self, plugin)?;
        transition.hooks_started();
        if let Err(error) = validate_plugin_config(plugin.plugin.as_ref(), &config).await {
            plugin.set_state(if lifecycle_error_is_poisoning(&error) {
                PluginInstanceState::Disabled
            } else {
                PluginInstanceState::Stopped
            })?;
            transition.finish();
            return Err(error);
        }
        plugin.replace_config(config)?;
        plugin.set_state(PluginInstanceState::Stopped)?;
        transition.finish();
        Ok(())
    }

    pub fn instance_snapshots(&self) -> Result<Vec<PluginInstanceSnapshot>, PluginHostError> {
        self.plugins
            .iter()
            .map(|plugin| {
                Ok(PluginInstanceSnapshot {
                    instance_id: plugin.instance_id.clone(),
                    manifest: plugin.manifest.clone(),
                    state: plugin.state()?,
                    config: plugin.config_snapshot()?,
                    schema: plugin.plugin.config_schema().cloned(),
                    granted_capabilities: plugin.granted_capabilities.clone(),
                })
            })
            .collect()
    }

    /// Stops an instance without deleting its state or persistent schedules.
    /// A stopped instance may be resumed; a poisoned/disabled instance requires recovery.
    pub async fn set_instance_enabled(
        &self,
        instance_id: &str,
        enabled: bool,
    ) -> Result<(), PluginHostError> {
        let plugin = self
            .plugins
            .iter()
            .find(|plugin| plugin.instance_id == instance_id)
            .ok_or_else(|| PluginHostError::Invocation {
                plugin_id: instance_id.to_owned(),
                message: "plugin instance is not registered".to_owned(),
            })?;
        let _lifecycle = plugin.lifecycle.lock().await;
        self.set_registered_enabled(plugin, enabled).await
    }

    async fn set_registered_enabled(
        &self,
        plugin: &Arc<RegisteredPlugin>,
        enabled: bool,
    ) -> Result<(), PluginHostError> {
        let instance_id = plugin.instance_id.as_str();
        let previous = plugin.state()?;
        if (enabled && previous == PluginInstanceState::Ready)
            || (!enabled && previous == PluginInstanceState::Stopped)
        {
            return Ok(());
        }
        if enabled
            && (previous == PluginInstanceState::Disabled
                || self.store.circuit_open(instance_id)?)
        {
            return Err(PluginHostError::Invocation {
                plugin_id: plugin.manifest.id.to_string(),
                message: "disabled plugin requires explicit recovery and a process restart"
                    .to_owned(),
            });
        }
        if previous == PluginInstanceState::Disabled {
            self.pause_plugin_schedules(instance_id)?;
            let _execution = plugin.execution.acquire_exclusive().await?;
            self.assets.remove_instance(instance_id);
            return Ok(());
        }
        plugin.require_state(if enabled {
            PluginInstanceState::Stopped
        } else {
            PluginInstanceState::Ready
        })?;
        let mut transition = LifecycleTransition::begin(self, plugin)?;
        let _execution = plugin.execution.acquire_exclusive().await?;
        // An invocation already in flight may have poisoned the instance while
        // we waited. Never invoke lifecycle hooks against that guest again.
        if plugin.state()? == PluginInstanceState::Disabled {
            self.pause_plugin_schedules(instance_id)?;
            self.assets.remove_instance(instance_id);
            return if enabled {
                plugin.require_state(PluginInstanceState::Draining)
            } else {
                Ok(())
            };
        }
        plugin.require_state(PluginInstanceState::Draining)?;
        let config = plugin.config_snapshot()?;
        transition.hooks_started();
        let result = if enabled {
            supervised_activate(plugin, config).await
        } else {
            self.pause_plugin_schedules(instance_id)?;
            self.assets.remove_instance(instance_id);
            supervised_shutdown(plugin).await
        };
        if let Err(error) = result {
            plugin.set_state(PluginInstanceState::Disabled)?;
            return Err(error);
        }
        // Disabling a poisoned instance must never make its Store reusable.
        if enabled {
            self.recover_instance_work(plugin).await?;
        } else {
            plugin.set_state(PluginInstanceState::Stopped)?;
        }
        transition.finish();
        Ok(())
    }

    pub(super) async fn recover_instance_work(
        &self,
        plugin: &Arc<RegisteredPlugin>,
    ) -> Result<(), PluginHostError> {
        // No workers start during replay, including schedule.create commands.
        self.recover_pending_commands(plugin)?;
        self.recover_pending_deliveries(plugin).await?;
        plugin.require_state(PluginInstanceState::Draining)?;
        // Recover and validate persisted rows before publishing Ready. The
        // later sweep is read-only and cannot reset an active worker's claim.
        self.store.recover_schedules(&plugin.instance_id)?;
        plugin.set_state(PluginInstanceState::Ready)?;
        self.resume_pending_schedules(plugin)
    }
}
