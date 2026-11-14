//! Safe full status projection without package IO, guest hooks or configuration output.
use super::{ManagementError, PluginManager, state_name};
use plugin_host::PluginInstanceState;
use serde::Serialize;
use std::collections::BTreeMap;

pub(crate) const MAX_PLUGINS: usize = 256;

#[derive(Serialize)]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct PluginStatus {
    instance_id: String,
    plugin_id: String,
    name: String,
    version: String,
    loaded_version: Option<String>,
    enabled: bool,
    runtime_state: &'static str,
    pending: bool,
    restart_required: bool,
    updated_at_ms: i64,
}

impl PluginManager {
    pub(crate) async fn status(&self) -> Result<(Vec<PluginStatus>, bool), ManagementError> {
        let manager = self.clone();
        let (send, receive) = tokio::sync::oneshot::channel();
        let worker = std::thread::Builder::new()
            .name("bkm-status-sample".to_owned())
            .spawn(move || {
                let result = (|| {
                    let runtime = manager.runtime.try_read().map_err(|error| match error {
                        std::sync::TryLockError::WouldBlock => ManagementError::Busy,
                        std::sync::TryLockError::Poisoned(_) => ManagementError::Store,
                    })?;
                    let store = &manager.store;
                    let loaded = runtime
                        .host
                        .instance_statuses(MAX_PLUGINS + 1)
                        .map_err(|_| ManagementError::Store)?;
                    let saved = store.installation_statuses(&loaded, MAX_PLUGINS + 1)?;
                    let mut views = BTreeMap::new();
                    for item in &loaded {
                        views.insert(
                            item.instance_id.clone(),
                            PluginStatus {
                                instance_id: item.instance_id.clone(),
                                plugin_id: item.plugin_id.clone(),
                                name: item.name.clone(),
                                version: item.version.clone(),
                                loaded_version: Some(item.version.clone()),
                                enabled: item.state == PluginInstanceState::Ready,
                                runtime_state: state_name(item.state),
                                pending: runtime.packages.contains_key(&item.instance_id),
                                restart_required: runtime.packages.contains_key(&item.instance_id),
                                updated_at_ms: 0,
                            },
                        );
                    }
                    for item in saved {
                        let snapshot = loaded.iter().find(|s| s.instance_id == item.instance_id);
                        let restart_required = runtime.restart_required.contains(&item.instance_id)
                            || snapshot.is_some_and(|_| {
                                runtime.packages.get(&item.instance_id)
                                    != Some(&item.package_sha256)
                                    || item.grants_match != Some(true)
                            });
                        let ready = snapshot.is_some_and(|s| s.state == PluginInstanceState::Ready);
                        views.insert(
                            item.instance_id.clone(),
                            PluginStatus {
                                instance_id: item.instance_id,
                                plugin_id: item.plugin_id,
                                name: item.name,
                                version: item.version,
                                loaded_version: snapshot.map(|s| s.version.clone()),
                                enabled: item.enabled,
                                runtime_state: snapshot
                                    .map_or("not_loaded", |s| state_name(s.state)),
                                pending: restart_required
                                    || ready != item.enabled
                                    || (item.enabled
                                        && snapshot.is_some()
                                        && item.config_matches != Some(true)),
                                restart_required,
                                updated_at_ms: item.updated_at_ms,
                            },
                        );
                    }
                    let truncated = views.len() > MAX_PLUGINS || loaded.len() > MAX_PLUGINS;
                    Ok((views.into_values().take(MAX_PLUGINS).collect(), truncated))
                })();
                let _ = send.send(result);
            })
            .map_err(|_| ManagementError::Store)?;
        let result = receive.await.map_err(|_| ManagementError::Store);
        worker.join().map_err(|_| {
            tracing::warn!("management status sample thread failed");
            ManagementError::Store
        })?;
        result?
    }
}
