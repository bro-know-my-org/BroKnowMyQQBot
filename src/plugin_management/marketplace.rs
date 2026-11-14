//! Configured catalogue only: no browser supplied URL or filesystem path.
use std::{path::PathBuf, sync::Arc};

use plugin_host::PluginInstallation;
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use super::{ManagementError, PluginManager};
use crate::plugin_marketplace::{MarketplaceClient, MarketplacePlugin};

#[derive(Clone)]
pub(crate) struct MarketplaceContext {
    pub(crate) database: PathBuf,
    pub(crate) declarations: Option<PathBuf>,
    pub(crate) source: String,
    pub(crate) client: Arc<MarketplaceClient>,
}

#[derive(Serialize)]
pub(crate) struct Catalogue {
    source: String,
    total: usize,
    offset: usize,
    plugins: Vec<CatalogueItem>,
}

#[derive(Serialize)]
pub(crate) struct CatalogueItem {
    #[serde(flatten)]
    plugin: MarketplacePlugin,
    selection: String,
    instance_id: String,
    installed: Option<Installed>,
    manageable: bool,
}

#[derive(Serialize)]
struct Installed {
    version: String,
    saved_version: Option<String>,
    loaded_version: Option<String>,
    restart_required: bool,
    enabled: bool,
    updated_at_ms: i64,
    granted_permissions: Vec<String>,
    runtime_state: &'static str,
}

pub(crate) fn selection(plugin: &MarketplacePlugin) -> Result<String, ManagementError> {
    let bytes = serde_json::to_vec(plugin).map_err(|_| ManagementError::InvalidPackage)?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

impl PluginManager {
    pub(crate) fn with_marketplace(
        mut self,
        database: PathBuf,
        declarations: Option<PathBuf>,
        source: String,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        self.marketplace = Some(MarketplaceContext {
            database,
            declarations,
            client: Arc::new(MarketplaceClient::new(&source)?),
            source,
        });
        Ok(self)
    }

    #[cfg(test)]
    pub(crate) fn use_marketplace_fixture(
        &mut self,
        fixture: crate::plugin_marketplace::MarketplaceFixture,
    ) {
        let context = self
            .marketplace
            .as_mut()
            .expect("configured fixture context");
        context.client = Arc::new(
            MarketplaceClient::new(&context.source)
                .unwrap()
                .with_fixture(fixture),
        );
    }

    pub(crate) fn marketplace_context(&self) -> Result<&MarketplaceContext, ManagementError> {
        self.marketplace
            .as_ref()
            .ok_or(ManagementError::MarketplaceUnavailable)
    }

    pub(crate) async fn catalogue(
        &self,
        query: &str,
        offset: usize,
        limit: usize,
        selector: Option<&str>,
    ) -> Result<Catalogue, ManagementError> {
        if query.len() > 128 || !(1..=50).contains(&limit) || offset > 10_000 {
            return Err(ManagementError::InvalidMarketRequest);
        }
        if let Some(selector) = selector {
            crate::plugin_marketplace::validate_selector(selector)
                .map_err(|_| ManagementError::InvalidMarketRequest)?;
        }
        let context = self.marketplace_context()?;
        let index = context
            .client
            .fetch_index()
            .await
            .map_err(|_| ManagementError::MarketplaceUnavailable)?;
        let found = if let Some(selector) = selector {
            vec![
                index
                    .find(selector)
                    .map_err(|_| ManagementError::InvalidMarketRequest)?,
            ]
        } else {
            index.search(query)
        };
        let total = found.len();
        let selected: Vec<_> = found
            .into_iter()
            .skip(offset)
            .take(limit)
            .cloned()
            .collect();
        let store = self.store.clone();
        let records = tokio::task::spawn_blocking(move || {
            selected
                .into_iter()
                .map(|plugin| {
                    let id = format!("{}/default", plugin.plugin_id);
                    let installed = store.installation(&id)?;
                    Ok((plugin, id, installed))
                })
                .collect::<Result<Vec<_>, plugin_host::StoreError>>()
        })
        .await
        .map_err(|_| ManagementError::Store)??;
        let runtime = self.snapshot()?;
        let snapshots = runtime
            .host
            .instance_snapshots()
            .map_err(|_| ManagementError::Store)?;
        let mut plugins = Vec::with_capacity(records.len());
        for (plugin, instance_id, record) in records {
            let snapshot = snapshots
                .iter()
                .find(|snapshot| snapshot.instance_id == instance_id);
            let manageable = !crate::plugins::is_reserved_bundled_instance_id(&instance_id)
                && !self.file_instances.contains(&instance_id)
                && (snapshot.is_none() || runtime.packages.contains_key(&instance_id));
            let installed = record
                .as_ref()
                .map(|record: &PluginInstallation| Installed {
                    version: record.version.clone(),
                    saved_version: Some(record.version.clone()),
                    loaded_version: snapshot.map(|s| s.manifest.version.clone()),
                    restart_required: super::restart_required(record, snapshot, &runtime),
                    enabled: record.enabled,
                    updated_at_ms: record.updated_at_ms,
                    granted_permissions: record.granted_permissions.clone(),
                    runtime_state: snapshot.map_or("not_loaded", |s| super::state_name(s.state)),
                })
                .or_else(|| {
                    snapshot.map(|s| Installed {
                        version: s.manifest.version.clone(),
                        saved_version: None,
                        loaded_version: Some(s.manifest.version.clone()),
                        restart_required: false,
                        enabled: s.state == plugin_host::PluginInstanceState::Ready,
                        updated_at_ms: 0,
                        granted_permissions: s.granted_capabilities.iter().cloned().collect(),
                        runtime_state: super::state_name(s.state),
                    })
                });
            plugins.push(CatalogueItem {
                selection: selection(&plugin)?,
                plugin,
                instance_id,
                installed,
                manageable,
            });
        }
        Ok(Catalogue {
            source: context.source.clone(),
            total,
            offset,
            plugins,
        })
    }
}
