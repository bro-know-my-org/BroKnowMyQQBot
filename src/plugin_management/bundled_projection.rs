//! Saved intent stays separate from the running process on every management surface.
use super::{
    ManagementError, PluginManager, PluginView, bundled::Catalogue, state_name,
    status::PluginStatus,
};
use crate::plugins::bundled::{self, Settings};
use plugin_host::PluginInstanceStatus;

impl PluginManager {
    pub(super) async fn bundled_plugin_views(&self) -> Result<Vec<PluginView>, ManagementError> {
        let Catalogue { plugins, .. } = self.bundled_catalogue().await?;
        Ok(plugins
            .into_iter()
            .filter(|p| p.enabled || p.loaded)
            .map(|p| PluginView {
                instance_id: p.instance_id.to_owned(),
                plugin_id: p.plugin_id,
                name: p.name,
                description: p.description,
                version: p.version.clone(),
                saved_version: Some(p.version.clone()),
                loaded_version: p.loaded.then_some(p.version),
                restart_required: p.restart_required,
                source: "内置插件".to_owned(),
                trust_level: "file-managed".to_owned(),
                signature_status: "n/a".to_owned(),
                enabled: p.enabled,
                runtime_state: p.runtime_state,
                editable: false,
                config: p.config,
                schema: p.schema,
                requested_permissions: p.requested_permissions.clone(),
                granted_permissions: if p.loaded {
                    p.requested_permissions
                } else {
                    Vec::new()
                },
                updated_at_ms: 0,
                pending: p.pending,
                error: None,
            })
            .collect())
    }

    pub(super) fn bundled_statuses(
        &self,
        loaded: &[PluginInstanceStatus],
    ) -> Result<(Vec<PluginStatus>, Option<String>), ManagementError> {
        if self.marketplace.is_none() {
            return Ok((Vec::new(), None));
        }
        let source = self.read_bundled_source(false)?;
        let saved =
            Settings::parse(&source.text).map_err(|()| ManagementError::BundledUnavailable)?;
        let mut rows = Vec::new();
        for definition in bundled::catalogue() {
            let live = loaded
                .iter()
                .find(|p| p.instance_id == definition.instance_id);
            let enabled = saved.selected.iter().any(|key| key == definition.key);
            if !enabled && live.is_none() {
                continue;
            }
            let config = saved
                .configs
                .get(definition.key)
                .cloned()
                .unwrap_or_default();
            let matches = live
                .map(|p| p.config_matches(&config))
                .transpose()
                .map_err(|_| ManagementError::Store)?;
            let pending = enabled != live.is_some() || (enabled && matches == Some(false));
            let manifest = definition.plugin.manifest();
            rows.push(PluginStatus {
                instance_id: definition.instance_id.to_owned(),
                plugin_id: manifest.id.to_string(),
                name: manifest
                    .metadata
                    .resolve("zh-CN")
                    .map_err(|_| ManagementError::Store)?
                    .name
                    .clone(),
                version: manifest.version.clone(),
                loaded_version: live.map(|p| p.version.clone()),
                enabled,
                runtime_state: live.map_or("not_loaded", |p| state_name(p.state)),
                pending,
                restart_required: pending,
                updated_at_ms: 0,
            });
        }
        Ok((rows, Some(source.revision)))
    }
}
